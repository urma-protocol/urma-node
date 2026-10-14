use super::{
    ServeOptions,
    inputs::{invalid, private_directory, read_archive_config, read_header_bootstrap, read_token},
    overlay::{attach_overlay, shutdown_overlay},
};
use crate::{
    Error,
    engine::Engine,
    model::Retention,
    peer::OverlayCommitSource,
    service::{Service, ShutdownHandle},
    source::{CommitSources, LitecoinSource, LocalRpcCommitSource, PublicCommitSource},
};
use std::{io::Write, path::PathBuf, sync::Arc};
use urma_chain::observation::Chain;
use urma_runtime::{endpoints, p2p::Anchor};

/// Runs the archive service in the foreground until it is asked to stop.
/// `on_bound` receives the stop handle once the control listener is bound,
/// before overlay startup and any chain sync, and installs whatever stop
/// signal the host has (process signals, or the Windows service manager).
pub fn serve(
    options: ServeOptions,
    on_bound: impl FnOnce(ShutdownHandle) -> Result<(), Error>,
    log: &mut impl Write,
) -> Result<(), Error> {
    let config = read_archive_config(&options.config)?;
    let chain = config.chain;
    let application_height = config.application_start.height;
    let mut engine = Engine::open(&options.data, config, None)?;
    if options.p2p {
        let source = open_chain_source(&options, chain, application_height)?;
        engine = engine.with_source(Arc::new(source))?;
    }
    let engine = Arc::new(engine);
    let token = read_token(&options.token_file)?;
    let retain = options.p2p && engine.status()?.retention == Retention::Unpruned;
    let mut service = Service::bind(options.listen, token, Arc::clone(&engine))?
        .with_cache_limit(options.cache_bytes)?;
    on_bound(service.shutdown_handle())?;
    if retain {
        let scratch = options.data.join("scratch");
        private_directory(&scratch)?;
        service = service.with_retention(scratch, options.object_timeout)?;
    }
    let overlay = (options.overlay, options.p2p);
    let attached = attach_overlay(service, &options.data, &engine, chain, overlay, log)?;
    let public = (options.p2p && options.commit_public).then(|| options.data.join("providers"));
    let local = match (options.p2p, options.commit_rpc) {
        (true, Some(rpc)) => Some(LocalRpcCommitSource::new(chain, rpc.url, rpc.cookie)),
        (true, None) | (false, None) => None,
        (false, Some(rpc)) => {
            return Err(Error::Invalid(format!(
                "commit RPC {} needs chain sync",
                rpc.url
            )));
        }
    };
    let sources = (local, attached.commit_source, public);
    let service = attach_commit_sources(attached.service, chain, sources)?;
    writeln!(
        log,
        "URMA Node listening at {} (authenticated loopback)",
        service.address()?
    )?;
    let outcome = service.run(options.p2p);
    let network_outcome = match attached.network {
        Some(network) => shutdown_overlay(network),
        None => Ok(()),
    };
    outcome?;
    network_outcome
}

fn open_chain_source(
    options: &ServeOptions,
    chain: Chain,
    height: u64,
) -> Result<LitecoinSource, Error> {
    let pinned = options.chain_peers.clone();
    let bootstrap = match &options.header_bootstrap {
        Some(path) => Some(read_header_bootstrap(path, chain, height)?),
        None => None,
    };
    let cache = options.data.join("headers");
    std::fs::create_dir_all(&cache)?;
    match bootstrap {
        Some(bootstrap) => LitecoinSource::open_bootstrap(&cache, bootstrap, pinned),
        None => LitecoinSource::open(
            chain,
            &cache,
            Anchor::embedded(chain).map_err(invalid)?,
            pinned,
        ),
    }
}

type Sources = (
    Option<LocalRpcCommitSource>,
    Option<OverlayCommitSource>,
    Option<PathBuf>,
);

fn attach_commit_sources(
    service: Service,
    chain: Chain,
    (local, overlay, public): Sources,
) -> Result<Service, Error> {
    let mut sources = CommitSources::new(chain);
    if let Some(local) = local {
        sources.push("local-rpc".into(), Box::new(local));
    }
    if let Some(overlay) = overlay {
        sources.push("overlay".into(), Box::new(overlay));
    }
    let has_public = !endpoints::defaults(chain).is_empty();
    if let Some(cache) = public.filter(|_cache| has_public) {
        private_directory(&cache)?;
        sources.push(
            "public".into(),
            Box::new(PublicCommitSource::new(chain, cache)),
        );
    }
    if sources.is_empty() {
        return Ok(service);
    }
    service.with_commit_sources(sources)
}
