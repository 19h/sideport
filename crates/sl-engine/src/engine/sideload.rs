//! Jobs that need engine state: Apple ID provisioning before export.

use super::Inner;
use super::provision::{self, ProvisionRequest};
use crate::error::{EngineError, Result};
use crate::job::JobContext;
use crate::pipeline::{self, IdentityPlan, SigningPlan};
use crate::types::{JobOutcome, JobSpec, SigningMode, Target};
use sl_apple::portal::Platform;
use std::sync::Arc;

pub(super) async fn run(inner: Arc<Inner>, context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    let SigningMode::AppleId { apple_id } = spec.signing.clone() else {
        return pipeline::run_export(context, spec).await;
    };

    if matches!(spec.target, Target::Device { .. }) {
        return Err(EngineError::Unsupported("device installation is not connected to the engine yet".into()));
    }

    pipeline::validate_options(&spec)?;

    let entitlements = spec.options.entitlements.as_deref().map(pipeline::load_entitlements).transpose()?;
    let inspected = pipeline::inspect_job(&context, &spec).await?;
    let summary = inspected.summary;

    let request = ProvisionRequest {
        apple_id,
        policy: spec.options.bundle_id.clone(),
        original_bundle_id: inspected.original_bundle_id,
        current_bundle_id: summary.bundle_id.clone(),
        app_name: summary.name.clone(),
        extensions: summary.extensions.iter().map(|extension| extension.bundle_id.clone()).collect(),
        device: None,
        tvos_for_apple_tv: spec.options.tvos_for_apple_tv,
        provision_extensions: spec.options.provision_extensions,
    };

    let provisioned = provision::provision(inner, context.clone(), request).await?;
    let path = pipeline::output_path(&context, &spec, &summary).await?;

    let plan = SigningPlan::Identity(Box::new(IdentityPlan {
        identity: provisioned.identity,
        profile: provisioned.profile,
        extension_profiles: provisioned.extension_profiles,
        bundle_id: provisioned.bundle_id,
        record_original_id: provisioned.record_original_id,
        device_udid: None,
        platform: match provisioned.platform {
            Platform::Ios => "iOS",
            Platform::Tvos => "tvOS",
        },
        entitlements,
    }));

    context.checkpoint()?;

    tokio::task::spawn_blocking(move || pipeline::export(context, spec, summary, path, plan))
        .await
        .map_err(|error| EngineError::Other(format!("export worker failed: {error}")))?
}
