use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(cci), None) = (args.next(), args.next()) else {
        eprintln!("usage: xr-intake <dump-file>");
        return ExitCode::from(1);
    };
    let Some(data) = xr_intake::data_dir(std::env::var_os("XR_DATA"), std::env::var_os("APPDATA")) else {
        // Refusals do not need a data folder, so check the file first.
        return match xr_intake::identify_hash(PathBuf::from(&cci).as_path()) {
            Err(e) => report(e),
            Ok(_) => {
                eprintln!("neither XR_DATA nor APPDATA is set");
                ExitCode::from(1)
            }
        };
    };
    match xr_intake::intake(&PathBuf::from(cci), &data) {
        Ok(out) => {
            println!("ready: {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => report(e),
    }
}

fn report(e: xr_intake::Error) -> ExitCode {
    match e {
        xr_intake::Error::Refused(r) => {
            eprintln!("refused: {r}");
            ExitCode::from(2)
        }
        xr_intake::Error::Other(m) => {
            eprintln!("error: {m}");
            ExitCode::from(1)
        }
    }
}
