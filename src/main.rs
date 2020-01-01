#![allow(dead_code)] // Shut up.

mod asset;
mod atlas;
mod game;
mod gml;
mod instance;
mod instancelist;
mod render;
mod types;
mod util;

use std::{env, fs, path::Path, process};

fn help(argv0: &str, opts: getopts::Options) {
    print!(
        "{}",
        opts.usage(&format!(
            "Usage: {} FILE [options]",
            match Path::new(argv0).file_name() {
                Some(file) => file.to_str().unwrap_or(argv0),
                None => argv0,
            }
        ))
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let process = args[0].clone();

    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "prints this help message");
    opts.optflag("s", "strict", "enable various data integrity checks");
    opts.optflag("t", "singlethread", "parse gamedata synchronously");
    opts.optflag("v", "verbose", "enables verbose logging");

    let matches = opts.parse(&args[1..]).unwrap_or_else(|f| {
        use getopts::Fail::*;
        match f {
            ArgumentMissing(arg) => eprintln!("missing argument {}", arg),
            UnrecognizedOption(opt) => eprintln!("unrecognized option {}", opt),
            OptionMissing(opt) => eprintln!("missing option {}", opt),
            OptionDuplicated(opt) => eprintln!("duplicated option {}", opt),
            UnexpectedArgument(arg) => eprintln!("unexpected argument {}", arg),
        }
        process::exit(1); // todo: dtors
    });

    if args.len() < 2 || matches.opt_present("h") {
        help(&process, opts);
        return;
    }

    let strict = matches.opt_present("s");
    let multithread = !matches.opt_present("t");
    let verbose = matches.opt_present("v");
    let input = {
        if matches.free.len() == 1 {
            &matches.free[0]
        } else if matches.free.len() > 1 {
            eprintln!("unexpected second input {}", matches.free[1]);
            process::exit(1); // todo: dtors
        } else {
            eprintln!("no input file");
            process::exit(1); // todo: dtors
        }
    };

    let mut file = fs::read(&input).unwrap_or_else(|e| {
        eprintln!("failed to open '{}': {}", input, e);
        process::exit(1); // todo: dtors
    });

    if verbose {
        println!("loading '{}'...", input);
    }

    #[rustfmt::skip]
    let assets = gm8exe::reader::from_exe(
        &mut file,                              // mut exe: AsRef<[u8]>
        if verbose {                            // logger: Option<Fn(&str)>
            Some(|s: &str| println!("{}", s))
        } else {
            None
        },
        strict,                                 // strict: bool
        multithread,                            // multithread: bool
    )
    .unwrap_or_else(|e| {
        eprintln!("failed to load '{}' - {}", input, e);
        exit(1);
    });

    game::launch(assets);
}
