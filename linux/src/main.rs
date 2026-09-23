mod config;
mod decode;
mod net;
mod proto;
mod setup;
mod ui;

use anyhow::Result;

const USAGE: &str = "\
sweeflow — stream Mac app windows into your Linux desktop

USAGE:
    sweeflow                 connect to the paired Mac and show its windows
    sweeflow setup USER@MAC  install/update the Mac host over SSH and pair with it
    sweeflow status          show configuration and pairing info
";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    ffmpeg_next::init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("run") => ui::run(config::Config::load()?),
        Some("setup") => setup::setup(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("status") => setup::status(),
        _ => {
            eprint!("{USAGE}");
            Ok(())
        }
    }
}
