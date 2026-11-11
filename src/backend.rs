//! Construction of the `object_store` S3 client from CLI/env configuration.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;

/// Everything needed to talk to an S3-compatible endpoint.
#[derive(Debug, Clone, Default)]
pub struct S3Options {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub allow_http: bool,
}

/// Builds an S3 client. Explicit options win over the environment
/// (`AWS_ACCESS_KEY_ID`, `AWS_PROFILE`, `~/.aws/config`, instance roles, ...).
pub fn build_store(options: &S3Options) -> Result<Arc<dyn ObjectStore>> {
    if options.bucket.trim().is_empty() {
        bail!("a bucket name is required (--bucket or an s3://bucket/prefix target)");
    }

    let mut builder = AmazonS3Builder::from_env().with_bucket_name(options.bucket.trim());

    if let Some(endpoint) = options.endpoint.as_deref() {
        builder = builder
            .with_endpoint(endpoint)
            // Custom endpoints (MinIO, Ceph, R2, OSS, ...) expect path-style addressing.
            .with_virtual_hosted_style_request(false)
            .with_allow_http(options.allow_http || endpoint.starts_with("http://"));
    } else if options.allow_http {
        builder = builder.with_allow_http(true);
    }

    match options.region.as_deref() {
        Some(region) => builder = builder.with_region(region),
        None if std::env::var_os("AWS_REGION").is_none() && std::env::var_os("AWS_DEFAULT_REGION").is_none() => {
            builder = builder.with_region("us-east-1")
        }
        None => {}
    }

    if let Some(value) = options.access_key_id.as_deref() {
        builder = builder.with_access_key_id(value);
    }
    if let Some(value) = options.secret_access_key.as_deref() {
        builder = builder.with_secret_access_key(value);
    }
    if let Some(value) = options.session_token.as_deref() {
        builder = builder.with_token(value);
    }

    let store = builder
        .build()
        .context("building the S3 client (check --endpoint/--region/credentials)")?;
    Ok(Arc::new(store))
}
