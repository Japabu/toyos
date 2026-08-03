use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process;

/// Bytes per line. Every other width below is derived from it, so xxd's
/// two-byte groups and 16-byte lines are one number here rather than four.
const LINE: usize = 16;
/// Columns the hex field occupies before the ASCII column: the digits, plus
/// one space between each pair of bytes.
const HEX_WIDTH: usize = LINE * 2 + LINE.div_ceil(2) - 1;

/// One read buffer, whole lines, never derived from the file's size or from
/// `-l`: a dump of a whole disk allocates what a dump of one byte does.
const BUF_BYTES: usize = 4096;
const _: () = assert!(BUF_BYTES % LINE == 0, "the buffer must hold whole lines");

pub fn main(args: Vec<String>) {
    let mut skip = 0u64;
    let mut length: Option<u64> = None;
    let mut path: Option<&str> = None;

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" => skip = number(args.next(), "-s"),
            "-l" => length = Some(number(args.next(), "-l")),
            _ if arg.starts_with('-') => refuse(format!("{arg}: unknown option")),
            _ => match path {
                None => path = Some(arg),
                Some(first) => refuse(format!("{arg}: one file at a time, already given {first}")),
            },
        }
    }

    let Some(path) = path else {
        eprintln!("Usage: hexdump [-s offset] [-l length] <file>");
        process::exit(1);
    };

    if let Err(refusal) = dump(Path::new(path), skip, length) {
        eprintln!("hexdump: {refusal}");
        process::exit(1);
    }
}

fn refuse(what: String) -> ! {
    eprintln!("hexdump: {what}");
    process::exit(1);
}

/// Decimal, or hexadecimal with an `0x` prefix — the two forms xxd takes.
fn number(arg: Option<&String>, flag: &str) -> u64 {
    let Some(text) = arg else { refuse(format!("{flag}: needs a number")) };
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    match parsed {
        Ok(n) => n,
        Err(_) => refuse(format!("{flag}: {text} is not a number")),
    }
}

fn dump(path: &Path, skip: u64, length: Option<u64>) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| format!("{}: {e}", path.display()))?.len();

    // Silence would read as "the file has nothing there", which is a different
    // claim from "you asked past the end".
    if skip > size {
        return Err(format!("{}: offset {skip} is past the end ({size} bytes)", path.display()));
    }
    file.seek(SeekFrom::Start(skip)).map_err(|e| format!("{}: {e}", path.display()))?;

    // The request is folded into what the file actually holds here, so the
    // loop below has a byte count and not a "no limit" marker to test against.
    let mut left = length.map_or(size - skip, |l| l.min(size - skip));

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut buf = vec![0u8; BUF_BYTES];
    let mut offset = skip;

    while left > 0 {
        let want = (BUF_BYTES as u64).min(left) as usize;
        // Exact, not short: `size` said these bytes are there, so a read that
        // stops early is the file disagreeing with its own length and not the
        // end of the dump.
        file.read_exact(&mut buf[..want]).map_err(|e| format!("{}: {e}", path.display()))?;
        for line in buf[..want].chunks(LINE) {
            write_line(&mut out, offset, line).map_err(|e| format!("stdout: {e}"))?;
            offset += line.len() as u64;
        }
        left -= want as u64;
    }

    out.flush().map_err(|e| format!("stdout: {e}"))
}

fn write_line(out: &mut impl Write, offset: u64, line: &[u8]) -> io::Result<()> {
    write!(out, "{offset:08x}: ")?;
    let mut width = 0;
    for (i, pair) in line.chunks(2).enumerate() {
        if i > 0 {
            write!(out, " ")?;
            width += 1;
        }
        for byte in pair {
            write!(out, "{byte:02x}")?;
            width += 2;
        }
    }
    for _ in width..HEX_WIDTH {
        write!(out, " ")?;
    }
    write!(out, "  ")?;
    for &byte in line {
        let c = if (0x20..=0x7e).contains(&byte) { byte as char } else { '.' };
        write!(out, "{c}")?;
    }
    writeln!(out)
}
