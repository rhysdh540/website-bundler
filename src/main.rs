use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;

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
    },
    Deploy {
        /// Directory to deploy. Must be a directory.
        #[arg(long)]
        dir: String,

        /// Cloudflare Pages project name to deploy to.
        #[arg(long)]
        project_name: String
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
        Cli::Dev {
            build,
            addr,
            debounce,
        } => website_bundler::dev_server::run(build, addr, debounce).await,
        Cli::Deploy {
            dir,
            project_name,
        } => {
            let account_id = std::env::var("CF_ACCOUNT_ID")
                .expect("CF_ACCOUNT_ID environment variable not set");
            let api_token = std::env::var("CF_API_TOKEN")
                .expect("CF_API_TOKEN environment variable not set");
            website_bundler::deploy::deploy(dir.into(), &project_name, &account_id, &api_token).await
        }
    }
}
