use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::Vfs;
use crate::drivers::{acpi, serial, xhci};
use crate::{console, process, keyboard, log};

pub fn run(vfs: &mut Vfs) -> ! {
    console::write_str(&format!("{}> ", vfs.cwd()));

    let mut line_buf = [0u8; 256];
    let mut line_len: usize = 0;

    // Text input mode state (for write/edit commands)
    let mut editing_file: Option<String> = None; // full path
    let mut text_buf: Vec<u8> = Vec::new();

    loop {
        xhci::poll_global();

        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\n' => {
                    console::putchar(b'\n');
                    serial::println("");

                    let input = core::str::from_utf8(&line_buf[..line_len])
                        .unwrap_or("")
                        .trim();

                    if let Some(ref path) = editing_file {
                        if input == "." {
                            let saved = vfs.write_file(path, &text_buf);
                            if saved {
                                log::println("File saved.");
                            } else {
                                log::println("Error: could not save file.");
                            }
                            editing_file = None;
                            text_buf.clear();
                        } else {
                            text_buf.extend_from_slice(&line_buf[..line_len]);
                            text_buf.push(b'\n');
                        }
                    } else if !input.is_empty() {
                        let (cmd, arg) = match input.find(' ') {
                            Some(pos) => (&input[..pos], input[pos + 1..].trim()),
                            None => (input, ""),
                        };
                        exec(vfs, cmd, arg, &mut editing_file, &mut text_buf);
                    }

                    line_len = 0;
                    if editing_file.is_some() {
                        console::write_str("| ");
                    } else {
                        console::write_str(&format!("{}> ", vfs.cwd()));
                    }
                }
                0x08 => {
                    if line_len > 0 {
                        line_len -= 1;
                        console::backspace();
                    }
                }
                ch => {
                    if line_len < line_buf.len() {
                        line_buf[line_len] = ch;
                        line_len += 1;
                        console::putchar(ch);
                    }
                }
            }
        } else {
            core::hint::spin_loop();
        }
    }
}

fn exec(
    vfs: &mut Vfs,
    cmd: &str,
    arg: &str,
    editing_file: &mut Option<String>,
    text_buf: &mut Vec<u8>,
) {
    match cmd {
        "help" => {
            log::println("Commands:");
            log::println("  ls [path]       List files");
            log::println("  cat <file>      Print file contents");
            log::println("  rm <file>       Delete a file");
            log::println("  write <file>    Create a file");
            log::println("  edit <file>     Edit a file");
            log::println("  cd <path>       Change directory");
            log::println("  pwd             Print working directory");
            log::println("  clear           Clear screen");
            log::println("  run <file>      Run an ELF program");
            log::println("  shutdown        Power off");
        }
        "clear" => console::clear(),
        "shutdown" => acpi::shutdown(),
        "pwd" => log::println(vfs.cwd()),
        "cd" => {
            let target = if arg.is_empty() { "/" } else { arg };
            if !vfs.cd(target) {
                log::println(&format!("cd: {}: no such directory", target));
            }
        }
        "ls" => match vfs.list(arg) {
            Ok(files) => {
                if files.is_empty() {
                    log::println("No files.");
                } else {
                    for (name, size) in &files {
                        if name.ends_with('/') {
                            log::println(&format!("  {}", name));
                        } else {
                            log::println(&format!("  {} ({} bytes)", name, size));
                        }
                    }
                }
            }
            Err(e) => log::println(&format!("ls: {}", e)),
        },
        "cat" => {
            if arg.is_empty() {
                log::println("Usage: cat <file>");
            } else {
                let (mount, file) = vfs.resolve_path(arg);
                if file.is_empty() {
                    log::println(&format!("cat: /{}: is a directory", mount));
                } else if let Some(data) = vfs.read_file(arg) {
                    if let Ok(text) = core::str::from_utf8(&data) {
                        log::println(text);
                    } else {
                        log::println(&format!("{}: {} bytes (binary)", file, data.len()));
                    }
                } else {
                    log::println(&format!("{}: file not found", arg));
                }
            }
        }
        "rm" => {
            if arg.is_empty() {
                log::println("Usage: rm <file>");
            } else {
                let (_, file) = vfs.resolve_path(arg);
                if file.is_empty() {
                    log::println("rm: cannot remove a mount point");
                } else if vfs.delete(arg) {
                    log::println(&format!("{}: deleted", file));
                } else {
                    log::println(&format!("{}: file not found", arg));
                }
            }
        }
        "write" => {
            if arg.is_empty() {
                log::println("Usage: write <file>");
            } else {
                let (mount, file) = vfs.resolve_path(arg);
                if file.is_empty() {
                    log::println("write: need a filename");
                } else if !vfs.mount_exists(&mount) {
                    log::println(&format!("write: /{}: no such directory", mount));
                } else {
                    log::println("Enter text (type . on a line by itself to save):");
                    *editing_file = Some(String::from(arg));
                    text_buf.clear();
                }
            }
        }
        "edit" => {
            if arg.is_empty() {
                log::println("Usage: edit <file>");
            } else {
                let (mount, file) = vfs.resolve_path(arg);
                if file.is_empty() {
                    log::println("edit: need a filename");
                } else if !vfs.mount_exists(&mount) {
                    log::println(&format!("edit: /{}: no such directory", mount));
                } else {
                    if let Some(data) = vfs.read_file(arg) {
                        if let Ok(text) = core::str::from_utf8(&data) {
                            log::println("Current contents:");
                            log::println(text);
                        } else {
                            log::println(&format!("{}: binary file, cannot edit", file));
                            return;
                        }
                    } else {
                        log::println("(new file)");
                    }
                    log::println("Enter new text (type . on a line by itself to save):");
                    *editing_file = Some(String::from(arg));
                    text_buf.clear();
                }
            }
        }
        "run" => {
            if arg.is_empty() {
                log::println("Usage: run <file>");
            } else {
                // Split arg into file + remaining args
                let (file, rest) = match arg.find(' ') {
                    Some(pos) => (&arg[..pos], arg[pos + 1..].trim()),
                    None => (arg, ""),
                };
                if let Some(data) = vfs.read_file(file) {
                    let mut args: Vec<&str> = Vec::new();
                    args.push(file);
                    if !rest.is_empty() {
                        args.extend(rest.split_whitespace());
                    }
                    let code = process::run(&data, &args);
                    crate::fd::close_all(vfs);
                    if code != 0 {
                        log::println(&format!("Process exited with code {}", code));
                    }
                } else {
                    log::println(&format!("{}: file not found", file));
                }
            }
        }
        _ => {
            // PATH: try to find the command in /initrd/
            let path = format!("/initrd/{}", cmd);
            if let Some(data) = vfs.read_file(&path) {
                let mut args: Vec<&str> = Vec::new();
                args.push(cmd);
                if !arg.is_empty() {
                    args.extend(arg.split_whitespace());
                }
                let code = process::run(&data, &args);
                crate::fd::close_all(vfs);
                if code != 0 {
                    log::println(&format!("Process exited with code {}", code));
                }
            } else {
                log::println(&format!("Unknown command: {}", cmd));
            }
        }
    }
}
