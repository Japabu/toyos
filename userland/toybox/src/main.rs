mod cat;
mod echo;
mod free;
mod grep;
mod locale;
mod ls;
mod mkdir;
mod net;
mod pwd;
mod rm;
mod screen;
mod shutdown;
mod spin;

macro_rules! commands {
    ($($name:ident),*) => {
        fn run(cmd: &str, args: Vec<String>) {
            match cmd {
                $(stringify!($name) => $name::main(args),)*
                _ => eprintln!("toybox: unknown command '{cmd}'"),
            }
        }

        fn install() {
            let names = &[$(stringify!($name)),*];
            for name in names {
                let link = format!("/bin/{name}");
                // Remove existing file/symlink if present
                std::fs::remove_file(&link).ok();
                std::os::toyos::fs::symlink("/bin/toybox", &link).unwrap_or_else(|e| {
                    eprintln!("toybox: failed to create symlink {link}: {e}");
                });
            }
        }
    };
}

commands!(cat, echo, free, grep, locale, ls, mkdir, net, pwd, rm, screen, shutdown, spin);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let invoked_as = std::path::Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("toybox");

    if invoked_as == "toybox" {
        if args.get(1).map(|s| s.as_str()) == Some("--install") {
            install();
            return;
        }
        // Subcommand mode: toybox ls -la
        if args.len() < 2 {
            eprintln!("Usage: toybox <command> [args...]");
            return;
        }
        let cmd_args = args[2..].to_vec();
        run(&args[1], cmd_args);
    } else {
        // Symlink mode: invoked as "ls -la"
        let cmd_args = args[1..].to_vec();
        run(invoked_as, cmd_args);
    }
}
