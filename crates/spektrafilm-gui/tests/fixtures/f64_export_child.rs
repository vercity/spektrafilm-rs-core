use std::{env, fs, path::Path, process, thread, time::Duration};

fn main() -> std::io::Result<()> {
    let args: Vec<_> = env::args_os().collect();
    if args.len() < 5 || args[1] != "process" || args[3] != "-o" {
        process::exit(24);
    }
    let path = Path::new(&args[4]);
    if path.extension().is_none_or(|extension| extension != "tiff") {
        process::exit(25);
    }
    let input = Path::new(&args[2]);
    match input.file_name().and_then(|name| name.to_str()) {
        Some("complete") => fs::write(path, b"complete")?,
        Some("fail") => {
            fs::write(path, b"partial")?;
            eprintln!("encoder failed");
            process::exit(23);
        }
        Some("staged-output-written") => {
            fs::write(path, b"partial")?;
            fs::write(input, b"")?;
            thread::sleep(Duration::from_secs(30));
        }
        _ => process::exit(26),
    }
    Ok(())
}
