mod app;
mod catalog;
mod delete;
mod layout;
mod lineedit;
mod loader;
mod makernote;
mod meta;
mod relocate;
mod render;
mod state;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use winit::event_loop::EventLoop;

/// Where the move screen suggests putting the culled shots.
const DEFAULT_DEST_ROOT: &str = "~/mnt/truenas/Pictures/Digital Photography/Raw Shots";

/// Quickly cull RAW+JPEG pairs: browse the JPEGs full screen, mark rejects, trash them in pairs.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Directory containing the RAW+JPEG pairs.
    dir: PathBuf,
    /// Run in a window instead of full screen.
    #[arg(short, long)]
    windowed: bool,
    /// Folder in which to suggest creating the dated destination folder once culling is done.
    /// Falls back to the folder containing DIR if it doesn't exist.
    #[arg(long, value_name = "DIR", default_value = DEFAULT_DEST_ROOT)]
    dest_root: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let dir = match args.dir.canonicalize() {
        Ok(d) if d.is_dir() => d,
        _ => {
            eprintln!("cull: {} is not a directory", args.dir.display());
            return ExitCode::FAILURE;
        }
    };
    let catalog = match catalog::Catalog::scan(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cull: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if catalog.shots.is_empty() {
        eprintln!("cull: no JPEGs found in {}", dir.display());
        return ExitCode::FAILURE;
    }
    let raws = catalog.shots.iter().filter(|s| s.has_raw()).count();
    println!("cull: {} shots ({} with RAW) in {}", catalog.shots.len(), raws, dir.display());
    if !catalog.orphan_raws.is_empty() {
        println!("cull: skipping {} RAW file(s) with no matching JPEG", catalog.orphan_raws.len());
    }

    let event_loop = match EventLoop::<()>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("cull: {e}");
            return ExitCode::FAILURE;
        }
    };
    let proxy = event_loop.create_proxy();
    let paths = catalog.shots.iter().map(|s| s.jpeg.clone()).collect();
    let wake = proxy.clone();
    let loader = loader::Loader::new(paths, move || {
        let _ = wake.send_event(());
    });
    let dest_root = relocate::expand(&args.dest_root);
    let mut app = app::App::new(dir, catalog, args.windowed, dest_root, proxy, loader);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("cull: {e}");
        return ExitCode::FAILURE;
    }
    if let Some(msg) = app.exit_message.take() {
        println!("{msg}");
    }
    ExitCode::SUCCESS
}
