//! lap24.hub: share the current folder over the LAN so VLC (PC, phone, TV)
//! can play the videos. HTTP + M3U playlist + Range seeking + DLNA/UPnP.

mod dlna;
mod http;
mod transcode;
mod util;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = "\
lap24.hub - share a video folder with VLC over the local network (LAN)

Usage: lap24.hub.exe [FOLDER] [--port N] [--name NAME] [--no-dlna]
                     [--no-transcode] [--encoder NAME] [--sub-lang TAG]

  FOLDER       Folder to share (default: current directory)
  --port N     HTTP port (default: 8080; tries the next port if busy)
  --name NAME  Name shown in VLC > Local Network (UPnP)
  --no-dlna    Disable UPnP/DLNA discovery
  --no-transcode  Never transcode, always send files as-is
  --sub-lang TAG  Subtitle shown first; burned into the video for Samsung
                  TVs, from Movie.TAG.srt
                  (e.g. --sub-lang ms for Movie.ms.srt)
  --encoder NAME  ffmpeg H.264 encoder (default: libx264; GPU options:
                  h264_nvenc, h264_qsv, h264_amf)

Transcoding: put ffmpeg.exe and ffprobe.exe next to lap24.hub.exe (or on
PATH). Videos a TV can't decode (HEVC/x265, 10-bit, VP9, AV1) are then
converted to H.264 while streaming. Other files are sent as-is.
  -h, --help   Show this help
";

struct Args {
    dir: Option<PathBuf>,
    port: Option<u16>,
    name: Option<String>,
    dlna: bool,
    transcode: bool,
    encoder: String,
    sub_lang: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { dir: None, port: None, name: None, dlna: true, transcode: true, encoder: "libx264".into(), sub_lang: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" | "/?" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-p" | "--port" => {
                let v = it.next().ok_or("--port needs a number")?;
                a.port = Some(v.parse().map_err(|_| format!("invalid port: {v}"))?);
            }
            "--name" => a.name = Some(it.next().ok_or("--name needs a value")?),
            "--no-dlna" => a.dlna = false,
            "--no-transcode" => a.transcode = false,
            "--encoder" => a.encoder = it.next().ok_or("--encoder needs a value")?,
            "--sub-lang" => a.sub_lang = Some(it.next().ok_or("--sub-lang needs a value, e.g. ms")?),
            s if s.starts_with('-') => return Err(format!("unknown option: {s}\n\n{HELP}")),
            s => a.dir = Some(PathBuf::from(s)),
        }
    }
    Ok(a)
}

/// Folder for converted videos: `.lap24hub-cache` inside the shared folder
/// (hidden from listings), or the temp folder if that isn't writable.
/// Caches from earlier names (`.layaraku-cache`, `.rustmedia-cache`) are
/// moved into it, never deleted; if one can't be renamed it is used as it is.
fn cache_dir(root: &std::path::Path) -> PathBuf {
    let writable = |d: &std::path::Path| {
        std::fs::create_dir_all(d).is_ok() && {
            let probe = d.join(".write-test");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        }
    };
    let preferred = migrate_all(root, ".", ".lap24hub-cache");
    if writable(&preferred) {
        return preferred;
    }
    let tmp = std::env::temp_dir();
    let fallback = migrate_all(&tmp, "", "lap24hub-cache");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Earlier product names whose cache folders are carried over, newest first.
const OLD_CACHE_NAMES: [&str; 2] = ["layaraku-cache", "rustmedia-cache"];

/// Move every old-named cache in `dir` into `new_name`; returns the folder to use.
fn migrate_all(dir: &std::path::Path, prefix: &str, new_name: &str) -> PathBuf {
    let mut target = dir.join(new_name);
    for old in OLD_CACHE_NAMES {
        target = migrate_cache(dir.join(format!("{prefix}{old}")), target);
    }
    target
}

/// Pick the cache folder to use: `new`, after moving the old cache (`old`)
/// into it. Nothing is deleted: if a move fails, the old folder (or file)
/// stays where it was and is used as it is.
fn migrate_cache(old: PathBuf, new: PathBuf) -> PathBuf {
    if !old.is_dir() {
        return new;
    }
    if !new.exists() {
        return match std::fs::rename(&old, &new) {
            Ok(()) => {
                println!(" Cache folder renamed: {} -> {}", old.display(), new.display());
                new
            }
            Err(e) => {
                println!(" Could not rename {} ({e}); using it as it is.", old.display());
                old
            }
        };
    }
    // Both exist: move over each old entry the new folder doesn't have yet.
    let mut left = 0;
    if let Ok(entries) = std::fs::read_dir(&old) {
        for e in entries.flatten() {
            let to = new.join(e.file_name());
            if to.exists() || std::fs::rename(e.path(), &to).is_err() {
                left += 1;
            }
        }
    }
    if left == 0 {
        let _ = std::fs::remove_dir(&old); // only succeeds when empty
    } else {
        println!(" {left} file(s) left in {} (not moved).", old.display());
    }
    new
}

/// Startup scan: queue every missing converted copy and report the totals.
fn prepare_all(ctx: &http::Ctx, tc: &'static transcode::Transcoder, videos: &[String]) {
    use transcode::CacheState;
    let (mut burn_ready, mut burn_queued, mut tv_ready, mut tv_queued, mut failed) = (0, 0, 0, 0, 0);
    let mut count = |st: CacheState, ready: &mut usize, queued: &mut usize| match st {
        CacheState::Ready(_) => *ready += 1,
        CacheState::Failed => failed += 1,
        CacheState::Waiting | CacheState::Converting(_) => *queued += 1,
    };
    let mut no_sub = Vec::new();
    if ctx.sub_lang.is_some() {
        for rel in videos {
            match ctx.burn_source(rel) {
                Some((full, sub)) => count(tc.queue_burn(&full, &sub, false), &mut burn_ready, &mut burn_queued),
                None => no_sub.push(rel.as_str()),
            }
        }
    }
    for rel in videos {
        let Some((full, _)) = util::safe_join(&ctx.root, rel) else { continue };
        if tc.needs_transcode(&full) {
            count(tc.enqueue(full, ctx.sibling_subs(rel), false), &mut tv_ready, &mut tv_queued);
        }
    }

    println!();
    println!(" Startup scan done ({} videos):", videos.len());
    if let Some(lang) = &ctx.sub_lang {
        println!(
            "   Samsung (subtitles burned in): {burn_ready} ready, {burn_queued} to convert, {} without a .{lang}.srt",
            no_sub.len()
        );
        for rel in no_sub.iter().take(5) {
            println!("     no subtitle: {rel}");
        }
        if no_sub.len() > 5 {
            println!("     ... and {} more", no_sub.len() - 5);
        }
    }
    println!("   TV (HEVC -> H.264 copy)      : {tv_ready} ready, {tv_queued} to convert");
    if failed > 0 {
        println!("   {failed} failed earlier in this run and will not be retried until restart");
    }
    if burn_queued + tv_queued > 0 {
        println!("   Converting in the background, one at a time. Videos can be played meanwhile.");
    } else {
        println!("   Everything is ready.");
    }
    println!();
}

/// Keep the console window open when started by double-click so errors can be read.
fn fail(msg: &str) -> ! {
    eprintln!("\nERROR: {msg}");
    eprintln!("Press Enter to exit...");
    let _ = std::io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

fn main() {
    let args = parse_args().unwrap_or_else(|e| fail(&e));
    let dir = match args.dir {
        Some(d) => d,
        None => std::env::current_dir().unwrap_or_else(|e| fail(&format!("cannot read current directory: {e}"))),
    };
    let root = dir
        .canonicalize()
        .unwrap_or_else(|e| fail(&format!("folder '{}' does not exist: {e}", dir.display())));
    if !root.is_dir() {
        fail(&format!("'{}' is not a folder", root.display()));
    }
    // canonicalize() on Windows yields `\\?\C:\...`; strip it for display.
    let shown_root = root.display().to_string().trim_start_matches(r"\\?\").to_string();

    // Bind HTTP.
    let (listener, port) = match args.port {
        Some(p) => match TcpListener::bind(("0.0.0.0", p)) {
            Ok(l) => (l, p),
            Err(e) => fail(&format!("cannot open port {p}: {e}")),
        },
        None => (8080..8100)
            .find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok().map(|l| (l, p)))
            .unwrap_or_else(|| fail("no free port between 8080-8099")),
    };

    // Initial scan for the summary.
    let mut media = Vec::new();
    http::walk_media(&root, "", &mut media, 0);
    let total_size: u64 = media.iter().map(|e| e.size).sum();

    let ip = util::lan_ip();
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PC".into());
    let folder_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| shown_root.clone());

    let device = if args.dlna {
        let name = args.name.clone().unwrap_or_else(|| format!("lap24.hub - {folder_name} ({host})"));
        let dev: &'static dlna::Device = Box::leak(Box::new(dlna::Device::new(name, &format!("{host}|{shown_root}"))));
        Some(dev)
    } else {
        None
    };

    let tc: Option<&'static transcode::Transcoder> = if args.transcode {
        transcode::Transcoder::detect(args.encoder.clone(), cache_dir(&root)).map(|t| &*Box::leak(Box::new(t)))
    } else {
        None
    };
    let ctx = Arc::new(http::Ctx {
        root: root.clone(),
        dlna: device,
        tc,
        sub_lang: args.sub_lang.clone(),
        clients: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    if let Some(tc) = tc {
        tc.start_worker();
        // Prepare every video at startup instead of waiting for a TV to ask:
        // first the Samsung copies with the --sub-lang subtitles burned in,
        // then the H.264 copies for TVs that can't play HEVC. Copies already
        // in the cache are skipped. A video someone starts watching jumps
        // to the front of the queue.
        let ctx = ctx.clone();
        let videos: Vec<String> = media
            .iter()
            .filter(|e| util::mime_for(&e.name).1 == util::Kind::Video)
            .map(|e| e.rel.clone())
            .collect();
        std::thread::spawn(move || prepare_all(&ctx, tc, &videos));
    }

    println!("============================================================");
    println!(" lap24.hub {}  -  media server for VLC", env!("CARGO_PKG_VERSION"));
    println!("============================================================");
    println!(" Folder   : {shown_root}");
    println!(
        " Found    : {} video/audio files ({:.2} GB)",
        media.len(),
        total_size as f64 / 1_073_741_824.0
    );
    println!();
    println!(" In VLC: Media > Open Network Stream, enter:");
    println!();
    println!("   http://{ip}:{port}/playlist.m3u      <- all videos (playlist)");
    println!("   http://{ip}:{port}/                  <- file list (web browser)");
    println!();
    println!(" Each player gets its own version of a video:");
    println!("   Samsung TV      : original file; with --sub-lang, a copy with those");
    println!("                     subtitles burned into the picture");
    println!("   TV with VLC     : H.264 copy when needed, all subtitles as tracks");
    println!("   PC/Mac with VLC : original file, full quality");
    println!(" A Mac or another PC using the playlist link: add ?for=pc to it.");
    println!();
    if let Some(dev) = device {
        match dlna::start_ssdp(dev, ip, port) {
            None => {
                println!(" UPnP/DLNA: on. In VLC (TV/phone) open Local Network /");
                println!("   Universal Plug'n'Play and choose \"{}\".", dev.name);
            }
            Some(w) => println!(" UPnP/DLNA: {w}"),
        }
        println!();
    }
    match tc {
        Some(t) => {
            println!(" Transcoding: on ({}, encoder {}).", t.ffmpeg_path().display(), t.encoder());
            println!("   At startup every video is checked and converted in the background,");
            println!("   one at a time, without waiting for a TV to ask:");
            if args.sub_lang.is_some() {
                println!("   - a copy with the --sub-lang subtitles burned in (Samsung), first");
            }
            println!("   - an H.264 copy of HEVC/x265, 10-bit, VP9 and AV1 videos (TV VLC)");
            println!("   Converted copies are kept in the folder below and reused next time:");
            println!("   {}", t.cache_dir().display().to_string().trim_start_matches(r"\\?\"));
            println!("   Videos still play before they are converted.");
        }
        None if args.transcode => {
            println!(" Transcoding: off (ffmpeg.exe + ffprobe.exe not found next to");
            println!("   lap24.hub.exe or on PATH). HEVC videos may not play on TVs.");
        }
        None => println!(" Transcoding: off (--no-transcode)."),
    }
    println!();
    if ip.is_loopback() {
        println!(" WARNING: no network detected. Connect to Wi-Fi first.");
    }
    println!(" The laptop and TV must be on the same Wi-Fi/network.");
    println!(" If Windows Firewall asks, choose 'Allow' (Private network).");
    println!(" Press Ctrl+C to stop.");
    println!("============================================================");

    http::run(ctx, listener);
}

#[cfg(test)]
mod tests {
    use super::migrate_cache;

    #[test]
    fn old_cache_is_moved_not_deleted() {
        let base = std::env::temp_dir().join(format!("lap24hub-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join(".rustmedia-cache"), base.join(".lap24hub-cache"));

        // Only the old folder: it is renamed.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.mp4"), b"a").unwrap();
        assert_eq!(migrate_cache(old.clone(), new.clone()), new);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new.join("a.mp4")).unwrap(), b"a");

        // Both exist: old files are moved in, existing new files win.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.mp4"), b"old").unwrap();
        std::fs::write(old.join("b.mp4"), b"b").unwrap();
        assert_eq!(migrate_cache(old.clone(), new.clone()), new);
        assert_eq!(std::fs::read(new.join("a.mp4")).unwrap(), b"a");
        assert_eq!(std::fs::read(new.join("b.mp4")).unwrap(), b"b");
        assert_eq!(std::fs::read(old.join("a.mp4")).unwrap(), b"old");

        // No old folder: the new path is used.
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(migrate_cache(old.clone(), new.clone()), new);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn both_old_names_are_carried_over() {
        let base = std::env::temp_dir().join(format!("lap24hub-mig2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for (dir, file) in [(".layaraku-cache", "l.mp4"), (".rustmedia-cache", "r.mp4")] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
            std::fs::write(base.join(dir).join(file), file).unwrap();
        }
        let got = super::migrate_all(&base, ".", ".lap24hub-cache");
        assert_eq!(got, base.join(".lap24hub-cache"));
        assert!(got.join("l.mp4").exists() && got.join("r.mp4").exists());
        assert!(!base.join(".layaraku-cache").exists() && !base.join(".rustmedia-cache").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
