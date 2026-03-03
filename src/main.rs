use std::net::SocketAddr;
use clap::Parser;
use anyhow::Result;

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
enum Cli {
    Bundle(website_bundler::BuildOptions),
    Dev {
        #[command(flatten)]
        build: website_bundler::BuildOptions,

        /// Address to bind the dev server to.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: SocketAddr,

        /// Time to wait after rebuilding before trying to rebuild again.
        #[arg(long, default_value = "300")]
        debounce: u64,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Bundle(opts) => {
            println!("in: {:?}, out: {:?}", opts.in_dir, opts.out_dir);
            println!("Built in {:?}", website_bundler::build_site(opts)?);
            Ok(())
        }
        Cli::Dev { build, addr, debounce } => {
            website_bundler::dev_server::run(build, addr, debounce).await
        }
    }
}