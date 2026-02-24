mod cat;
mod echo;
mod grep;
mod keyboard;
mod ls;
mod pwd;
mod rm;
mod shutdown;

macro_rules! commands {
    ($($name:ident),*) => {
        fn run(cmd: &str, args: Vec<String>) {
            match cmd {
                $(stringify!($name) => $name::main(args),)*
                _ => eprintln!("toybox: unknown command '{cmd}'"),
            }
        }
    };
}

commands!(cat, echo, grep, keyboard, ls, pwd, rm, shutdown);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let invoked_as = std::path::Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("toybox");

    if invoked_as == "toybox" {
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
