use crate::{BuildOptions, build_site};
use anyhow::{Context, Result};
use axum::Router;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tower_http::services::{ServeDir, ServeFile};

pub async fn run(opts: BuildOptions, addr: SocketAddr, debounce: u64) -> Result<()> {
    unsafe {
        std::env::set_var("DEV", "true");
    }

    let debounce = Duration::from_millis(debounce);

    print!("building...");
    let initial = build_site(opts.clone()).context("initial build failed")?;
    println!("done in {:?}", initial);

    let (rebuild_tx, rebuild_rx) = mpsc::channel::<()>(1);

    {
        let _watcher_site = watch(opts.in_dir.clone(), rebuild_tx.clone(), debounce)
            .context("failed to start site file watcher")?;

        let _watcher_include = watch(opts.include_dir.clone(), rebuild_tx.clone(), debounce)
            .context("failed to start include file watcher")?;

        tokio::spawn(rebuild_worker(rebuild_rx, opts.clone()));

        let app = Router::new().fallback_service(
            ServeDir::new(&opts.out_dir).fallback(ServeFile::new(opts.out_dir.join("404.html"))),
        );

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind dev server")?;

        println!("Serving {} at http://{}", opts.out_dir.display(), addr);
        println!(
            "Watching {} and {}",
            opts.in_dir.display(),
            opts.include_dir.display()
        );
        println!("Output: {}", opts.out_dir.display());
        println!("Press Ctrl+C to stop");

        let _keep_alive = (_watcher_site, _watcher_include);

        axum::serve(listener, app).await?;
    }

    Ok(())
}

async fn rebuild_worker(mut rx: mpsc::Receiver<()>, opts: BuildOptions) {
    while rx.recv().await.is_some() {
        print!("rebuilding...");
        let opts_clone = opts.clone();
        let res = tokio::task::spawn_blocking(move || build_site(opts_clone)).await;
        match res {
            Ok(Ok(dur)) => println!("done in {:?}", dur),
            Ok(Err(e)) => {
                println!();
                eprintln!("build failed: {e:#}")
            }
            Err(e) => {
                println!();
                eprintln!("build task failed: {e:#}")
            }
        }

        while rx.try_recv().is_ok() {
            // drain all pending rebuilds
        }
    }
}

pub fn watch(
    in_dir: PathBuf,
    rebuild_tx: mpsc::Sender<()>,
    debounce: Duration,
) -> Result<RecommendedWatcher> {
    let last_sent = Arc::new(Mutex::new(
        Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
    ));

    let last_sent_cb = last_sent.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: std::result::Result<notify::Event, notify::Error>| match res {
            Ok(ev) => {
                let is_junk = ev.paths.iter().any(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n == ".DS_Store" || n.starts_with("._") || n.starts_with('.')
                    })
                });
                if is_junk {
                    return;
                }

                use notify::event::{EventKind, ModifyKind};
                let meaningful = matches!(
                    ev.kind,
                    EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Modify(ModifyKind::Name(_))
                );

                if meaningful {
                    let mut last = last_sent_cb.lock().unwrap();
                    if last.elapsed() < debounce {
                        return;
                    }
                    *last = Instant::now();

                    let _ = rebuild_tx.try_send(());
                }
            }
            Err(e) => {
                eprintln!("watch error: {e}");
            }
        },
        Config::default(),
    )?;

    watcher.watch(&in_dir, RecursiveMode::Recursive)?;
    Ok(watcher)
}
