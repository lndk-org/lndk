mod internal {
    #![allow(unused_imports)]
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

use home::home_dir;
use internal::*;
use lndk::lnd::{build_seed_from_lnd_node, get_lnd_client, validate_lnd_creds, LndCfg};
use lndk::offers::handler::OfferHandler;
use lndk::server::LNDKServer;
use lndk::{
    lndkrpc, setup_logger, Cfg, LifecycleSignals, LndkOnionMessenger, DEFAULT_CONFIG_FILE_NAME,
    DEFAULT_DATA_DIR, DEFAULT_LNDK_DIR, DEFAULT_LOG_FILE, DEFAULT_SERVER_HOST, DEFAULT_SERVER_PORT,
    TLS_CERT_FILENAME, TLS_KEY_FILENAME,
};
use lndkrpc::offers_server::OffersServer;
use log::{error, info};
use rcgen::{generate_simple_self_signed, CertifiedKey, Error as RcgenError};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{create_dir_all, metadata, set_permissions, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use tokio::select;
use tokio::signal::unix::SignalKind;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic_lnd::lnrpc::GetInfoRequest;

#[macro_use]
extern crate configure_me;

/// An error that occurs when generating TLS credentials.
#[derive(Debug)]
pub enum CertificateGenFailure {
    RcgenError(RcgenError),
    IoError(std::io::Error),
}

impl Display for CertificateGenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateGenFailure::RcgenError(e) => {
                write!(f, "Error generating TLS certificate: {e:?}")
            }
            CertificateGenFailure::IoError(e) => write!(f, "IO error: {e:?}"),
        }
    }
}

impl Error for CertificateGenFailure {}

// If a tls cert/key pair doesn't already exist, generate_tls_creds creates the tls cert/key pair
// required to secure connections to LNDK's gRPC server. By default they are stored in ~/.lndk.
pub fn generate_tls_creds(
    data_dir: PathBuf,
    tls_ips_string: Option<String>,
) -> Result<(), CertificateGenFailure> {
    let cert_path = data_dir.join(TLS_CERT_FILENAME);
    let key_path = data_dir.join(TLS_KEY_FILENAME);
    let tls_ips = collect_tls_ips(tls_ips_string);

    // Did we have to generate a new key? In that case we also need to regenerate the certificate.
    if !key_path.exists() || !cert_path.exists() {
        log::debug!("Generating fresh TLS credentials in {data_dir:?}");
        let mut subject_alt_names = vec!["localhost".to_string()];
        if let Some(ips) = tls_ips {
            for ip in ips {
                subject_alt_names.push(ip);
            }
        };

        let CertifiedKey {
            cert,
            signing_key: key_pair,
        } = generate_simple_self_signed(subject_alt_names)
            .map_err(CertificateGenFailure::RcgenError)?;

        // Create the tls files. Make sure the key is user-readable only:
        let mut file = File::create(&key_path).map_err(CertificateGenFailure::IoError)?;
        let mut perms = metadata(&key_path)
            .map_err(CertificateGenFailure::IoError)?
            .permissions();
        perms.set_mode(0o600);
        set_permissions(&key_path, perms).map_err(CertificateGenFailure::IoError)?;

        file.write_all(key_pair.serialize_pem().as_bytes())
            .map_err(CertificateGenFailure::IoError)?;
        drop(file);

        std::fs::write(&cert_path, cert.pem()).map_err(CertificateGenFailure::IoError)?;
    };

    Ok(())
}

// Read the existing tls credentials from disk.
pub fn read_tls(data_dir: PathBuf) -> Result<Identity, std::io::Error> {
    let cert = std::fs::read_to_string(data_dir.join(TLS_CERT_FILENAME))?;
    let key = std::fs::read_to_string(data_dir.join(TLS_KEY_FILENAME))?;

    Ok(Identity::from_pem(cert, key))
}

// The user first passes in the tls ips as a comma-deliminated string into LNDK. Here we turn that
// string into a Vec.
fn collect_tls_ips(tls_ips_str: Option<String>) -> Option<Vec<String>> {
    tls_ips_str.map(|tls_ips_str| tls_ips_str.split(',').map(|str| str.to_owned()).collect())
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    let paths = get_conf_file_paths();
    let config = Config::including_optional_config_files(paths)
        .unwrap_or_exit()
        .0;
    let data_dir = create_data_dir(&config.data_dir).map_err(|e| {
        println!("Error creating LNDK's data dir: {:?}", e);
    })?;
    let log_file = config.log_file.map(PathBuf::from).or(config
        .data_dir
        .map(|data_dir| PathBuf::from(data_dir).join(DEFAULT_LOG_FILE)));

    setup_logger(config.log_level, log_file)?;

    let creds = validate_lnd_creds(
        config.cert_path,
        config.cert_pem,
        config.macaroon_path,
        config.macaroon_hex,
    )
    .map_err(|e| {
        error!("Error validating config: {e}.");
    })?;
    let address = config.address.clone();
    let lnd_args = LndCfg::new(config.address, creds.clone());

    let (shutdown, listener) = triggered::trigger();
    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener: listener.clone(),
    };
    let args = Cfg {
        lnd: lnd_args,
        signals,
        skip_version_check: config.skip_version_check,
        rate_limit_count: config.rate_limit_count,
        rate_limit_period_secs: config.rate_limit_period_secs,
    };

    let mut sigterm_stream = tokio::signal::unix::signal(SignalKind::terminate())
        .map_err(|e| error!("Error initializing sigterm signal: {e}."))?;
    let mut sigint_stream = tokio::signal::unix::signal(SignalKind::interrupt())
        .map_err(|e| error!("Error initializing sigint signal: {e}."))?;

    tokio::spawn(async move {
        tokio::select! {
            _ = sigint_stream.recv() => {
                info!("Received CTRL-C, shutting down..");
                shutdown.trigger();
            }
            _ = sigterm_stream.recv() => {
                info!("Received SIGTERM, shutting down..");
                shutdown.trigger();
            }
        }
    });

    let response_invoice_timeout = config.response_invoice_timeout;
    if let Some(timeout) = response_invoice_timeout {
        if timeout == 0 {
            error!("Error: response_invoice_timeout must be more than 0 seconds.");
            exit(1);
        }
    };

    let mut client = get_lnd_client(args.lnd.clone()).expect("failed to connect to lnd");
    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .expect("failed to get info")
        .into_inner();

    let grpc_host = match config.grpc_host {
        Some(host) => host,
        None => DEFAULT_SERVER_HOST.to_string(),
    };
    let grpc_port = match config.grpc_port {
        Some(port) => port,
        None => DEFAULT_SERVER_PORT,
    };
    let addr = format!("{grpc_host}:{grpc_port}").parse().map_err(|e| {
        error!("Error parsing API address: {e}");
    })?;
    let lnd_tls_str = creds.get_certificate_string()?;

    // The user passed in a TLS cert to help us establish a secure connection to LND. But now we
    // need to generate a TLS credentials for connecting securely to the LNDK server.
    generate_tls_creds(data_dir.clone(), config.tls_ip).map_err(|e| {
        error!("Error generating tls credentials: {e}");
    })?;
    let identity = read_tls(data_dir).map_err(|e| {
        error!("Error reading tls credentials: {e}");
    })?;

    let mut signer = client.clone();
    let seed = build_seed_from_lnd_node(&mut signer).await.map_err(|e| {
        error!("Error creating seed: {:?}", e);
    })?;
    let handler = Arc::new(OfferHandler::new(
        config.response_invoice_timeout,
        Some(seed),
        Some(client.clone()),
    ));
    let messenger = LndkOnionMessenger::new();

    let server = LNDKServer::new(
        Arc::clone(&handler),
        &info.identity_pubkey,
        lnd_tls_str,
        address,
    )
    .await;

    let server_fut = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))
        .expect("couldn't configure tls")
        .add_service(OffersServer::new(server))
        .serve_with_shutdown(addr, listener);

    info!("Starting lndk's grpc server at address {grpc_host}:{grpc_port}");

    select! {
       _ = messenger.run(args, Arc::clone(&handler)) => {
           info!("Onion messenger completed");
       },
       result2 = server_fut => {
            match result2 {
                Ok(_) => info!("API completed"),
                Err(e) => error!("Error running API: {}", e),
            };
       },
    }

    Ok(())
}

// Creates lndk's data directory at the specified directory, or ~/.lndk/data if not specified.
// Process must have write access to the directory.
fn create_data_dir(data_dir: &Option<String>) -> Result<PathBuf, std::io::Error> {
    let path = match data_dir {
        Some(dir) => PathBuf::from(&dir),
        None => get_default_lnkd_dir_path().join(DEFAULT_DATA_DIR),
    };

    create_dir_all(&path)?;

    Ok(path)
}

fn get_default_lnkd_dir_path() -> PathBuf {
    home_dir().unwrap().join(DEFAULT_LNDK_DIR)
}

fn get_conf_file_paths() -> Vec<OsString> {
    let default_lndk_config_path = get_default_lnkd_dir_path()
        .join(DEFAULT_CONFIG_FILE_NAME)
        .into_os_string();
    let current_dir_conf_file = Path::new("./")
        .join(DEFAULT_CONFIG_FILE_NAME)
        .into_os_string();

    let paths = vec![current_dir_conf_file, default_lndk_config_path];
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_tls_ips() {
        // Test that it returns a vector of one element if only one ip is provided.
        let tls_ips_str = Some("192.168.0.1".to_string());
        let tls_ips = collect_tls_ips(tls_ips_str);
        assert!(tls_ips.is_some());
        assert!(tls_ips.as_ref().unwrap().len() == 1);

        // If no ip is provided, collect_tls_ips should return None.
        assert!(collect_tls_ips(None).is_none());

        // If two ips are provided, a vector of length two should be returned.
        let tls_ips_str = Some("192.168.0.1,192.168.0.3".to_string());
        let tls_ips = collect_tls_ips(tls_ips_str);
        assert!(tls_ips.is_some());
        assert!(tls_ips.as_ref().unwrap().len() == 2);
    }
}
