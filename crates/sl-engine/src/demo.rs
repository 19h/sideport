//! Simulated backend for UI development, screenshots and demos (`EngineConfig { demo: true, .. }`).
//! Nothing here touches the network, devices or disk (except reading the selected file's size).

use crate::error::{EngineError, Result};
use crate::job::{Fact, JobContext, PromptKind, PromptReply, Stage, TeamChoice};
use crate::types::*;
use chrono::{Duration, Utc};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct Demo {
    state: Arc<Mutex<DemoState>>,
}

#[derive(Debug)]
struct DemoState {
    accounts: Vec<AccountSummary>,
    installations: Vec<Installation>,
    next_installation: i64,
}

fn free_team() -> TeamSummary {
    TeamSummary { team_id: "A1B2C3D4E5".into(), name: "Jane Appleseed (Personal Team)".into(), kind: TeamKind::Free }
}

fn org_team() -> TeamSummary {
    TeamSummary { team_id: "Z9Y8X7W6V5".into(), name: "Example Labs Ltd".into(), kind: TeamKind::Organization }
}

impl Demo {
    pub(crate) fn new() -> Self {
        let now = Utc::now();
        let account = AccountSummary {
            apple_id: "jane@example.com".into(),
            teams: vec![free_team()],
            default_team: Some(free_team().team_id),
            has_session: true,
            remembers_password: true,
            last_login: Some(now - Duration::hours(30)),
        };
        let spec = |name: &str| JobSpec {
            source: PathBuf::from(format!("/Users/jane/Builds/{name}.ipa")),
            target: Target::Device { udid: "00008120-001A2B3C4D5E6F70".into(), prefer_network: false },
            signing: SigningMode::AppleId { apple_id: "jane@example.com".into() },
            options: AppOptions { track_for_refresh: true, remove_watch_app: true, ..Default::default() },
        };
        let installations = vec![
            Installation {
                id: 1,
                app_name: "Field Notes".into(),
                bundle_id: "com.example.fieldnotes.A1B2C3D4E5".into(),
                original_bundle_id: "com.example.fieldnotes".into(),
                version: Some("2.4.1".into()),
                device_udid: "00008120-001A2B3C4D5E6F70".into(),
                device_name: "Jane's iPhone".into(),
                apple_id: "jane@example.com".into(),
                team_id: "A1B2C3D4E5".into(),
                installed_at: now - Duration::days(5),
                expires_at: Some(now + Duration::days(2) - Duration::hours(3)),
                auto_refresh: true,
                last_error: None,
                consecutive_failures: 0,
                icon_png: None,
                spec: spec("FieldNotes"),
            },
            Installation {
                id: 2,
                app_name: "Trailhead".into(),
                bundle_id: "org.example.trailhead.A1B2C3D4E5".into(),
                original_bundle_id: "org.example.trailhead".into(),
                version: Some("0.9.0".into()),
                device_udid: "00008120-001A2B3C4D5E6F70".into(),
                device_name: "Jane's iPhone".into(),
                apple_id: "jane@example.com".into(),
                team_id: "A1B2C3D4E5".into(),
                installed_at: now - Duration::days(1),
                expires_at: Some(now + Duration::days(6)),
                auto_refresh: false,
                last_error: Some("Device was not reachable over Wi-Fi".into()),
                consecutive_failures: 1,
                icon_png: None,
                spec: spec("Trailhead"),
            },
        ];
        Self { state: Arc::new(Mutex::new(DemoState { accounts: vec![account], installations, next_installation: 3 })) }
    }

    pub(crate) fn devices(&self) -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                udid: "00008120-001A2B3C4D5E6F70".into(),
                name: "Jane's iPhone".into(),
                product_type: "iPhone15,2".into(),
                model_name: Some("iPhone 14 Pro".into()),
                os_version: "18.2".into(),
                device_class: "iPhone".into(),
                connections: vec![Connection::Usb, Connection::Network],
                paired: true,
            },
            DeviceInfo {
                udid: "00008103-000E4C1A0C38801E".into(),
                name: "Studio iPad".into(),
                product_type: "iPad13,4".into(),
                model_name: Some("iPad Pro 11-inch (3rd generation)".into()),
                os_version: "17.7".into(),
                device_class: "iPad".into(),
                connections: vec![Connection::Network],
                paired: true,
            },
        ]
    }

    pub(crate) fn accounts(&self) -> Vec<AccountSummary> {
        self.state.lock().accounts.clone()
    }

    pub(crate) async fn test_anisette(&self, setting: AnisetteSetting) -> Result<String> {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        match setting {
            AnisetteSetting::Local => Ok("MacBookPro18,3 with serial number C02XXXXXXXXX running macOS 15.1".into()),
            AnisetteSetting::Remote { url } if url.starts_with("https://") => {
                Ok("iMac20,1 with serial number C02YYYYYYYYY running macOS 13.6".into())
            }
            AnisetteSetting::Remote { .. } => Err(EngineError::Anisette("URL must use https://".into())),
        }
    }

    pub(crate) async fn login(
        &self,
        ctx: JobContext,
        apple_id: String,
        password: Option<String>,
        remember: bool,
    ) -> Result<AccountSummary> {
        ctx.stage(Stage::Authenticating);
        let (_password, remember) = match password {
            Some(p) => (p, remember),
            None => match ctx.ask(PromptKind::Password { apple_id: apple_id.clone(), remember }).await? {
                PromptReply::Text { value, remember } => (value, remember),
                _ => return Err(EngineError::Cancelled),
            },
        };
        ctx.info("Prefetching anisette…");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        ctx.fact(Fact::AnisetteDevice("MacBookPro18,3 running macOS 15.1".into()));
        ctx.info(format!("Authenticating {apple_id}"));
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        loop {
            let reply = ctx
                .ask(PromptKind::SecondFactor {
                    apple_id: apple_id.clone(),
                    destination: "your trusted devices".into(),
                    code_length: 6,
                    can_request_sms: true,
                })
                .await?;
            match reply {
                PromptReply::Text { value, .. } if value == "000000" => {
                    ctx.warn("Verification code incorrect; try entering it again.");
                }
                PromptReply::Text { .. } => break,
                PromptReply::RequestSms => ctx.info("Sent a code to •••• •••• 42"),
                _ => return Err(EngineError::Cancelled),
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let teams = if apple_id.contains("org") { vec![free_team(), org_team()] } else { vec![free_team()] };
        let account = AccountSummary {
            apple_id: apple_id.clone(),
            default_team: Some(teams[0].team_id.clone()),
            teams,
            has_session: true,
            remembers_password: remember,
            last_login: Some(Utc::now()),
        };
        let mut st = self.state.lock();
        st.accounts.retain(|a| a.apple_id != apple_id);
        st.accounts.push(account.clone());
        ctx.info("Signed in");
        Ok(account)
    }

    pub(crate) fn logout(&self, apple_id: &str) {
        self.state.lock().accounts.retain(|a| a.apple_id != apple_id);
    }

    pub(crate) fn certificates(&self, _apple_id: &str) -> Vec<CertificateSummary> {
        vec![
            CertificateSummary {
                serial: "5A1D2C3B4E5F6071".into(),
                name: "Apple Development: Jane Appleseed (8FQ2X3LM9K)".into(),
                machine_name: Some("Jane's MacBook Pro".into()),
                expires: Some(Utc::now() + Duration::days(300)),
                is_ours: true,
            },
            CertificateSummary {
                serial: "1B2C3D4E5F607182".into(),
                name: "Apple Development: Jane Appleseed (8FQ2X3LM9K)".into(),
                machine_name: Some("Old Laptop".into()),
                expires: Some(Utc::now() + Duration::days(90)),
                is_ours: false,
            },
        ]
    }

    pub(crate) fn app_ids(&self, _apple_id: &str) -> Vec<AppIdSummary> {
        vec![AppIdSummary {
            app_id_id: "QX7Y5V4T3S".into(),
            identifier: "com.example.fieldnotes.A1B2C3D4E5".into(),
            name: "Field Notes".into(),
            expires: Some(Utc::now() + Duration::days(4)),
        }]
    }

    pub(crate) fn inspect(&self, path: PathBuf) -> AppSummary {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "App".into());
        let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(48_234_112);
        AppSummary {
            name: stem.clone(),
            bundle_id: format!("com.example.{}", stem.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "")),
            version: Some("142".into()),
            short_version: Some("3.2.0".into()),
            minimum_os: Some("16.0".into()),
            icon_png: None,
            extensions: vec![
                ExtensionInfo {
                    file_name: "Widgets.appex".into(),
                    bundle_id: "com.example.app.widgets".into(),
                    display_name: Some("Widgets".into()),
                },
                ExtensionInfo {
                    file_name: "Share.appex".into(),
                    bundle_id: "com.example.app.share".into(),
                    display_name: Some("Share".into()),
                },
            ],
            has_watch_app: true,
            file_size,
            encrypted: false,
            device_family: vec![1, 2],
            warnings: Vec::new(),
            path,
        }
    }

    pub(crate) async fn run_job(&self, ctx: JobContext, spec: JobSpec) -> Result<JobOutcome> {
        let app = self.inspect(spec.source.clone());
        let step = |ms: u64| tokio::time::sleep(std::time::Duration::from_millis(ms));

        ctx.stage(Stage::Preparing);
        ctx.info(format!("Reading {}", spec.source.display()));
        step(300).await;

        let mut bundle_id = app.bundle_id.clone();
        let mut expires = None;
        if let SigningMode::AppleId { apple_id } = &spec.signing {
            ctx.stage(Stage::Authenticating);
            ctx.info(format!("Using saved session for {apple_id}"));
            step(500).await;
            ctx.stage(Stage::Provisioning);
            let teams = if apple_id.contains("org") { vec![free_team(), org_team()] } else { vec![free_team()] };
            let team = if teams.len() > 1 {
                let choices = teams.iter().cloned().map(|team| TeamChoice { team }).collect();
                match ctx.ask(PromptKind::ChooseTeam { apple_id: apple_id.clone(), teams: choices }).await? {
                    PromptReply::Choice(i) if i < teams.len() => teams[i].clone(),
                    _ => return Err(EngineError::Cancelled),
                }
            } else {
                teams[0].clone()
            };
            ctx.fact(Fact::Team(team.clone()));
            if team.kind == TeamKind::Free && spec.options.bundle_id == BundleIdPolicy::Auto {
                bundle_id = format!("{bundle_id}.{}", team.team_id);
            }
            if let BundleIdPolicy::Custom(id) = &spec.options.bundle_id {
                bundle_id = id.clone();
            }
            ctx.fact(Fact::BundleId(bundle_id.clone()));
            for msg in [
                "Registering device",
                "Checking signing certificate",
                "Registering app ID",
                "Downloading provisioning profile",
            ] {
                ctx.checkpoint()?;
                ctx.info(msg);
                step(350).await;
            }
            ctx.fact(Fact::AppIdQuota { remaining: 8, next_release: Some(Utc::now() + Duration::days(3)) });
            let exp = Utc::now() + Duration::days(7);
            ctx.fact(Fact::ProfileExpiry { expires: exp, ttl_days: Some(7) });
            expires = Some(exp);
        }

        ctx.stage(Stage::Patching);
        if spec.options.remove_watch_app && app.has_watch_app {
            ctx.info("Removing watch app");
        }
        step(250).await;

        if !matches!(spec.signing, SigningMode::Original | SigningMode::Unsigned) {
            ctx.stage(Stage::Signing);
            for i in 0..=20u64 {
                ctx.checkpoint()?;
                ctx.progress(i, 20);
                step(60).await;
            }
        }

        match &spec.target {
            Target::ExportIpa { path } => {
                ctx.stage(Stage::Packaging);
                let out = match path {
                    Some(p) => p.clone(),
                    None => match ctx
                        .ask(PromptKind::SaveFile { suggested_name: format!("{} Signed.ipa", app.name) })
                        .await?
                    {
                        PromptReply::Path(p) => p,
                        _ => return Err(EngineError::Cancelled),
                    },
                };
                for i in 0..=10u64 {
                    ctx.progress(i, 10);
                    step(60).await;
                }
                ctx.stage(Stage::Done);
                ctx.info(format!("Saved {}", out.display()));
                Ok(JobOutcome { bundle_id, exported_to: Some(out), expires, installation_id: None })
            }
            Target::Device { udid, .. } => {
                ctx.stage(Stage::Uploading);
                let total = app.file_size.max(1);
                for i in 0..=40u64 {
                    ctx.checkpoint()?;
                    ctx.progress(total * i / 40, total);
                    step(45).await;
                }
                ctx.stage(Stage::Installing);
                for pct in (0..=100u64).step_by(5) {
                    ctx.checkpoint()?;
                    ctx.progress(pct, 100);
                    step(50).await;
                }
                ctx.stage(Stage::Done);
                ctx.info("Done.");
                let mut installation_id = None;
                if spec.options.track_for_refresh
                    && let SigningMode::AppleId { apple_id } = &spec.signing
                {
                    let mut st = self.state.lock();
                    let id = st.next_installation;
                    st.next_installation += 1;
                    st.installations.push(Installation {
                        id,
                        app_name: app.name.clone(),
                        bundle_id: bundle_id.clone(),
                        original_bundle_id: app.bundle_id.clone(),
                        version: app.short_version.clone(),
                        device_udid: udid.clone(),
                        device_name: "Jane's iPhone".into(),
                        apple_id: apple_id.clone(),
                        team_id: free_team().team_id,
                        installed_at: Utc::now(),
                        expires_at: expires,
                        auto_refresh: true,
                        last_error: None,
                        consecutive_failures: 0,
                        icon_png: None,
                        spec: spec.clone(),
                    });
                    installation_id = Some(id);
                }
                Ok(JobOutcome { bundle_id, exported_to: None, expires, installation_id })
            }
        }
    }

    pub(crate) fn installations(&self) -> Vec<Installation> {
        self.state.lock().installations.clone()
    }

    pub(crate) fn set_auto_refresh(&self, id: i64, enabled: bool) {
        if let Some(i) = self.state.lock().installations.iter_mut().find(|i| i.id == id) {
            i.auto_refresh = enabled;
        }
    }

    pub(crate) fn forget(&self, id: i64) {
        self.state.lock().installations.retain(|i| i.id != id);
    }

    pub(crate) async fn refresh(&self, ctx: JobContext, id: i64) -> Result<JobOutcome> {
        let spec = self
            .state
            .lock()
            .installations
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.spec.clone())
            .ok_or_else(|| EngineError::Other(format!("unknown installation {id}")))?;
        let mut spec = spec;
        spec.options.track_for_refresh = false;
        let outcome = self.run_job(ctx, spec).await?;
        if let Some(i) = self.state.lock().installations.iter_mut().find(|i| i.id == id) {
            i.installed_at = Utc::now();
            i.expires_at = outcome.expires;
            i.last_error = None;
            i.consecutive_failures = 0;
        }
        Ok(outcome)
    }

    pub(crate) fn device_apps(&self, _udid: &str) -> Vec<DeviceApp> {
        vec![
            DeviceApp {
                bundle_id: "com.example.fieldnotes.A1B2C3D4E5".into(),
                name: "Field Notes".into(),
                version: Some("2.4.1".into()),
                is_developer_app: true,
            },
            DeviceApp {
                bundle_id: "org.example.trailhead.A1B2C3D4E5".into(),
                name: "Trailhead".into(),
                version: Some("0.9.0".into()),
                is_developer_app: true,
            },
        ]
    }

    pub(crate) fn device_profiles(&self, _udid: &str) -> Vec<DeviceProfile> {
        vec![DeviceProfile {
            uuid: "3F2504E0-4F89-11D3-9A0C-0305E82C3301".into(),
            name: "iOS Team Provisioning Profile: com.example.fieldnotes.A1B2C3D4E5".into(),
            app_id: Some("com.example.fieldnotes.A1B2C3D4E5".into()),
            team_id: Some("A1B2C3D4E5".into()),
            expires: Some(Utc::now() + Duration::days(2)),
            is_free: true,
        }]
    }
}
