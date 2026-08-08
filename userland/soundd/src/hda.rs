//! soundd as the driver of an Intel HDA controller.
//!
//! `specs/hda-driver-plan.md` §4.1: the kernel brought the controller up, owns
//! the buffer descriptor list and the interrupt, and answers five register
//! writes and two reads. Everything that is a *decision* is here and in
//! `toyos-hda` — which codecs answered, which pin, which converter, the
//! amplifiers, EAPD, the format — and every one of those decisions is a pure
//! function this file calls.
//!
//! What this file itself owns is the I/O: one verb over the immediate-command
//! registers, and the stream's run bit.

use toyos::HdaDev;
use toyos_abi::hda::HdaInfo;
use toyos_abi::syscall::{RegWidth, SyscallError};
use toyos_hda::graph::Codec;
use toyos_hda::path::{OutputPath, PathError};
use toyos_hda::verb::{Address, Node, Response, Verb};
use toyos_hda::{config, probe};

/// The controller's immediate-command registers, as byte offsets into the
/// register window. The kernel's allow-list carries the same three numbers and
/// refuses everything else.
const IMMEDIATE_COMMAND: u32 = 0x60;
const IMMEDIATE_RESPONSE: u32 = 0x64;
const IMMEDIATE_STATUS: u32 = 0x68;

const IMMEDIATE_BUSY: u32 = 1 << 0;
const IMMEDIATE_RESULT_VALID: u32 = 1 << 1;

/// Stream descriptor fields, relative to the descriptor the kernel reported.
const SD_CTL: u32 = 0x00;
const SD_CTL_TAG: u32 = 0x02;
const SD_FMT: u32 = 0x12;

const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_FEIE: u32 = 1 << 3;
const SD_CTL_DEIE: u32 = 1 << 4;

/// How many times a verb's completion is polled before the controller is called
/// silent.
///
/// Policy: the specification completes an immediate command in one codec frame,
/// about 21 µs at 48 kHz, and each poll here is a syscall. A driver that spun
/// forever on a controller with no verb interface would take the machine's
/// audio down with a hang instead of a refusal.
const VERB_POLLS: u32 = 4096;

pub struct Hda {
    dev: HdaDev,
    info: HdaInfo,
    /// Set once the engine has been told to run, so a stop and a resume are one
    /// register write each and not one per period.
    running: bool,
}

/// Why this machine's HDA controller cannot carry audio. Each is a line soundd
/// prints before falling back to the null sink — "no sound" without which of
/// these it was is a report nobody can act on.
pub enum Refusal {
    NoDevice(SyscallError),
    /// The kernel's answers stopped making sense, which is a bug here or there
    /// and never a property of the machine.
    Kernel(SyscallError),
    /// No codec `STATESTS` named answered a verb.
    NoCodec,
    /// Every codec answered and none offers an output a human can hear.
    NoOutput(toyos_hda::PathError),
    /// The converter behind the chosen pin does not offer the one rate this
    /// pipeline runs at.
    Rate,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice(e) => write!(f, "no HDA controller the kernel bound ({e})"),
            Self::Kernel(e) => write!(f, "the kernel refused a call this driver has to make ({e})"),
            Self::NoCodec => write!(f, "no codec STATESTS named answered a verb"),
            Self::NoOutput(PathError::NoOutputPin { codecs }) => {
                write!(f, "no output a human can hear, on codec")?;
                for (i, address) in codecs.iter().enumerate() {
                    write!(f, "{}{address}", if i == 0 { " " } else { ", " })?;
                }
                Ok(())
            }
            Self::NoOutput(PathError::Cycle { pin, at }) => {
                write!(f, "pin {:#04x}'s connection list leads back to {:#04x}", pin.0, at.0)
            }
            Self::NoOutput(PathError::OutsideGroup { pin, named }) => write!(
                f,
                "pin {:#04x} names node {:#04x}, which its function group never declared",
                pin.0, named.0
            ),
            Self::NoOutput(PathError::NoConverter { pin }) => {
                write!(f, "no converter behind pin {:#04x}", pin.0)
            }
            Self::Rate => write!(
                f,
                "the converter does not offer {} Hz at {} bits",
                config::RATE,
                config::WIDTH
            ),
        }
    }
}

impl Hda {
    /// Claim the controller, walk its codecs, choose an output and configure it.
    pub fn claim() -> Result<(Self, OutputPath, u8), Refusal> {
        let dev = HdaDev::open().map_err(Refusal::NoDevice)?;
        let info = dev.info().map_err(Refusal::Kernel)?;
        let mut hda = Hda { dev, info, running: false };

        let found = probe::enumerate(&mut hda, info.statests);
        let mut codecs: Vec<Codec> = Vec::new();
        for entry in found {
            match entry {
                Ok(codec) => {
                    eprintln!(
                        "soundd: hda codec{} vendor={:04x} device={:04x}, {} function group(s)",
                        codec.address,
                        codec.vendor,
                        codec.device,
                        codec.groups.len()
                    );
                    codecs.push(codec);
                }
                Err((address, fault)) => {
                    eprintln!("soundd: hda codec{address} answered nothing usable ({fault:?})")
                }
            }
        }
        if codecs.is_empty() {
            return Err(Refusal::NoCodec);
        }

        let path = toyos_hda::find_output_path(&codecs).map_err(Refusal::NoOutput)?;
        let (format, channels) = config::format(&codecs, &path).ok_or(Refusal::Rate)?;
        eprintln!(
            "soundd: hda codec{} group {:#04x} converter {:#04x} -> pin {:#04x} ({}), \
             headphone {}, format {:#06x} ({} Hz {} ch {}-bit)",
            path.codec,
            path.group.0,
            path.converter.0,
            path.output.node.0,
            path.device.name(),
            match &path.headphone {
                Some(hp) => alloc_node(hp.node),
                None => String::from("none"),
            },
            format,
            config::RATE,
            channels,
            config::WIDTH,
        );

        let verbs = config::verbs(&codecs, &path, format, info.stream_tag)
            .expect("the path names a codec this walk produced");
        let sent = verbs.len();
        for verb in verbs {
            hda.send(verb);
        }

        // The tag before the format, and both before the engine is ever told to
        // run: a descriptor that starts with neither plays whatever the last
        // owner of the stream left in it.
        hda.write(SD_CTL_TAG, RegWidth::U8, (info.stream_tag as u32) << 4)
            .map_err(Refusal::Kernel)?;
        hda.write(SD_FMT, RegWidth::U16, format as u32).map_err(Refusal::Kernel)?;
        eprintln!("soundd: hda path configured in {sent} verbs, stream tag {}", info.stream_tag);
        Ok((hda, path, channels))
    }

    pub fn info(&self) -> HdaInfo {
        self.info
    }

    pub fn dev(&self) -> &HdaDev {
        &self.dev
    }

    /// Start the engine, which is one register write and only on the edge.
    ///
    /// There is no per-period submit: `SDnLVI` and the descriptor list are the
    /// kernel's and the engine cycles them unaided, so a period costs this
    /// driver a read of the completion record and no register access at all.
    pub fn start(&mut self) {
        if self.running {
            return;
        }
        if let Err(e) = self.write(SD_CTL, RegWidth::U8, SD_CTL_RUN | SD_CTL_IOCE | SD_CTL_FEIE | SD_CTL_DEIE) {
            panic!("soundd: hda could not start its stream: {e:?}");
        }
        self.running = true;
    }

    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        if let Err(e) = self.write(SD_CTL, RegWidth::U8, SD_CTL_IOCE | SD_CTL_FEIE | SD_CTL_DEIE) {
            panic!("soundd: hda could not stop its stream: {e:?}");
        }
        self.running = false;
    }

    fn write(&self, field: u32, width: RegWidth, value: u32) -> Result<(), SyscallError> {
        self.dev.reg_write(self.info.stream_offset + field, width, value)
    }

    /// One verb over the immediate-command registers: two writes, a bounded
    /// poll, and a read.
    ///
    /// CORB/RIRB would batch several verbs behind one ring-pointer write, and
    /// there is nothing here to batch for: verbs are sent at claim time and on a
    /// jack poll, never in the audio path. What the ring would buy is a
    /// syscall this driver spends about a hundred times per boot.
    fn send(&mut self, verb: Verb) -> Option<Response> {
        let dev = &self.dev;
        if !self.settles(|status| status & IMMEDIATE_BUSY == 0) {
            return None;
        }
        dev.reg_write(IMMEDIATE_STATUS, RegWidth::U16, IMMEDIATE_RESULT_VALID).ok()?;
        dev.reg_write(IMMEDIATE_COMMAND, RegWidth::U32, verb.raw()).ok()?;
        dev.reg_write(IMMEDIATE_STATUS, RegWidth::U16, IMMEDIATE_BUSY).ok()?;
        if !self.settles(|status| {
            status & IMMEDIATE_BUSY == 0 && status & IMMEDIATE_RESULT_VALID != 0
        }) {
            return None;
        }
        let response = dev.reg_read(IMMEDIATE_RESPONSE, RegWidth::U32).ok()?;
        dev.reg_write(IMMEDIATE_STATUS, RegWidth::U16, IMMEDIATE_RESULT_VALID).ok()?;
        Response::new(response)
    }

    fn settles(&self, ready: impl Fn(u32) -> bool) -> bool {
        for _ in 0..VERB_POLLS {
            match self.dev.reg_read(IMMEDIATE_STATUS, RegWidth::U16) {
                Ok(status) if ready(status) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}

impl probe::Verbs for Hda {
    fn get(&mut self, codec: Address, node: Node, verb: u16, payload: u8) -> Option<Response> {
        self.send(Verb::short(codec, node, verb, payload))
    }
}

fn alloc_node(node: Node) -> String {
    format!("{:#04x}", node.0)
}
