use std::{env, path::Path, process};

use rustos_ruidl_compiler::{load_schema, resolve_to_cache};

fn main() {
    if let Err(error) = run() {
        eprintln!("ruidl: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<String> = env::args().collect();
    match arguments.as_slice() {
        [_, command, input, cache, target] if command == "resolve" => {
            let schema = load_schema(Path::new(input))?;
            let result = resolve_to_cache(&schema, Path::new(cache), target)?;
            println!("{}", result.path.display());
            Ok(())
        }
        [_, flag] if flag == "--help" || flag == "-h" => {
            print_help();
            Ok(())
        }
        _ => Err(String::from(
            "usage: rustos-ruidl resolve <library.rune|schema.rune-abi> <cache-dir> <target-abi>",
        )),
    }
}

fn print_help() {
    println!(
        "rustos-ruidl — compiler RUIDL и content-addressed SDK cache\n\n\
         usage:\n  rustos-ruidl resolve <library.rune|schema.rune-abi> <cache-dir> <target-abi>\n\n\
         Для установленной DLL schema всегда читается из проверенного RUNE record INTERFACE_SCHEMA."
    );
}
