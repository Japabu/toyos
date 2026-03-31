mod cat;
mod echo;
mod free;
mod grep;
mod locale;
mod ls;
mod mkdir;
mod net;
mod ps;
mod pwd;
mod rm;
mod screen;
mod shutdown;
mod spin;
mod stats;
mod tone;

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

commands!(cat, echo, free, grep, locale, ls, mkdir, net, ps, pwd, rm, screen, shutdown, spin, stats, tone);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let invoked_as = std::path::Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("toybox");

    if invoked_as == "toybox" {
        if args.len() < 2 {
            eprintln!("Usage: toybox <command> [args...]");
            return;
        }
        run(&args[1], args[2..].to_vec());
    } else {
        run(invoked_as, args[1..].to_vec());
    }
}
