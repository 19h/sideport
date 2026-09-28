//! Injection sources and the custom icon through the rendered editor: a local `.deb`, a typed URL
//! and the Substrate special resolved against wiremock, and a PNG icon, all reaching an exported
//! app. GPUI's test platform does not implement the open panel, so the tests hand chosen paths to
//! the completion the picker calls; every other step clicks rendered controls.

use super::*;
use image::{Rgba, RgbaImage};
use std::io::Write;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const CUSTOM_COLOR: Rgba<u8> = Rgba([200, 30, 40, 255]);

/// A minimal `.deb` whose `data.tar.gz` holds one MobileSubstrate dylib named `dylib`.
fn tweak_deb(directory: &Path, file_name: &str, dylib: &str) -> PathBuf {
    let payload = common::macho();
    let mut tar = tar::Builder::new(Vec::new());

    let mut folder = tar::Header::new_gnu();
    folder.set_entry_type(tar::EntryType::Directory);
    folder.set_mode(0o755);
    folder.set_size(0);
    tar.append_data(&mut folder, "Library/MobileSubstrate/DynamicLibraries", std::io::empty()).expect("folder");

    let mut file = tar::Header::new_gnu();
    file.set_entry_type(tar::EntryType::Regular);
    file.set_mode(0o644);
    file.set_size(payload.len() as u64);
    let member = format!("Library/MobileSubstrate/DynamicLibraries/{dylib}");
    tar.append_data(&mut file, member, payload.as_slice()).expect("dylib");

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar.into_inner().expect("tar")).expect("gzip");
    let data = encoder.finish().expect("gzip");

    let mut archive = ar::Builder::new(Vec::new());
    archive.append(&ar::Header::new(b"debian-binary".to_vec(), 4), &b"2.0\n"[..]).expect("version");
    archive.append(&ar::Header::new(b"data.tar.gz".to_vec(), data.len() as u64), data.as_slice()).expect("data");

    let deb = directory.join(file_name);
    fs::write(&deb, archive.into_inner().expect("ar")).expect("deb");

    deb
}

/// Serves a tweak for a typed URL and the Substrate special's index page and package.
fn tweak_host(root: &Path) -> (tokio::runtime::Runtime, MockServer) {
    let remote = fs::read(tweak_deb(root, "remote.deb", "Remote.dylib")).expect("remote package");
    let substrate = fs::read(tweak_deb(root, "substrate.deb", "Substrate.dylib")).expect("substrate package");

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(MockServer::start());

    runtime.block_on(async {
        let index = ResponseTemplate::new(200).set_body_string("<span>latest\">0.9.7000</span>");
        Mock::given(method("GET")).and(path("/package/mobilesubstrate/")).respond_with(index).mount(&server).await;

        let package = ResponseTemplate::new(200).set_body_bytes(substrate);
        let package_path = "/debs/mobilesubstrate_0.9.7000_iphoneos-arm.deb";
        Mock::given(method("GET")).and(path(package_path)).respond_with(package).mount(&server).await;

        let tweak = ResponseTemplate::new(200).set_body_bytes(remote);
        Mock::given(method("GET")).and(path("/tweaks/remote.deb")).respond_with(tweak).mount(&server).await;
    });

    (runtime, server)
}

/// A synthetic app declaring `AppIcon` at 20 and 40 px.
fn app_with_icons(root: &Path) -> PathBuf {
    let bundle = root.join("Icons.app");
    common::synthetic_bundle(&bundle, "com.example.icons", "Icons", "APPL");

    let mut info = common::info("com.example.icons", "Icons", "APPL");
    info.insert("CFBundleIconFiles".into(), plist::Value::Array(vec!["AppIcon".into()]));
    common::write_info(&bundle, info);

    for (file_name, size) in [("AppIcon.png", 20), ("AppIcon@2x.png", 40)] {
        RgbaImage::from_pixel(size, size, Rgba([10, 20, 30, 255])).save(bundle.join(file_name)).expect("icon");
    }

    bundle
}

fn injection_sources(view: &Entity<Sideport>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
    cx.read(|cx| view.read(cx).draft.options.injections.iter().map(|injection| injection.source.clone()).collect())
}

fn set_icon(view: &Entity<Sideport>, cx: &mut VisualTestContext, icon: &Path) {
    let icon = icon.to_path_buf();

    cx.update(|_, cx| view.update(cx, |view, cx| view.set_custom_icon(icon, cx)));
}

#[gpui::test]
async fn injections_from_files_urls_and_specials_and_a_custom_icon_reach_the_export(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (_runtime, host) = tweak_host(temporary.path());
    let source = app_with_icons(temporary.path());
    let local = tweak_deb(temporary.path(), "local.deb", "Local.dylib");
    let destination = temporary.path().join("injected.ipa");

    let icon = temporary.path().join("Custom.png");
    let not_png = temporary.path().join("Notes.png");
    RgbaImage::from_pixel(64, 64, CUSTOM_COLOR).save(&icon).expect("custom icon");
    fs::write(&not_png, b"not an image").expect("text");

    let sources = sl_acquire::SpecialSources {
        substrate_index: format!("{}/package/mobilesubstrate/", host.uri()),
        substrate_deb_template: format!("{}/debs/mobilesubstrate_{{version}}_iphoneos-arm.deb", host.uri()),
        ..sl_acquire::SpecialSources::default()
    };
    let config = EngineConfig { special_sources: Some(sources), ..EngineConfig::default() };
    let (view, mut cx) = window(isolated(temporary.path(), &FakeDevice::iphone(UDID), config), cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    until(&mut cx, &view, "inspection", |view| view.app.is_some() && !view.busy).await;
    click(&mut cx, "mode:Ad-hoc signed");

    // A local package, as the picker hands it over.
    cx.update(|_, cx| view.update(cx, |view, cx| view.add_injection_sources(vec![local.clone()], cx)));

    // Typed sources must be http(s) URLs with a host.
    let url_field = cx.read(|cx| view.read(cx).fields.injection_url.clone());

    for invalid in ["ftp://example.test/tweak.deb", "https://", "tweak.deb"] {
        set_input(&mut cx, &url_field, invalid);
        click(&mut cx, "add-injection-url");

        assert!(cx.read(|cx| view.read(cx).error.is_some()), "{invalid} is refused");
        assert_eq!(cx.read(|cx| url_field.read(cx).value().to_string()), invalid, "the field keeps it for editing");
    }

    let remote = format!("{}/tweaks/remote.deb", host.uri());
    set_input(&mut cx, &url_field, &remote);
    click(&mut cx, "add-injection-url");
    assert_eq!(cx.read(|cx| url_field.read(cx).value().to_string()), "", "an added URL clears the field");

    // Specials toggle: the spoofer is added and removed again.
    click(&mut cx, "special:spoofer");
    click(&mut cx, "special:substrate");
    click(&mut cx, "special:spoofer");

    let expected = [local.clone(), PathBuf::from(&remote), PathBuf::from("///special/substrate")];
    assert_eq!(injection_sources(&view, &mut cx), expected);
    assert!(rendered(&mut cx, &format!("remove-injection:{remote}")));

    // Only a PNG becomes the icon; it can be cleared and chosen again.
    set_icon(&view, &mut cx, &not_png);
    assert!(cx.read(|cx| view.read(cx).error.clone()).is_some_and(|error| error.contains("not a PNG")));
    assert_eq!(cx.read(|cx| view.read(cx).draft.options.icon.clone()), None);

    set_icon(&view, &mut cx, &icon);
    assert!(rendered(&mut cx, "custom-icon"));

    click(&mut cx, "clear-icon");
    assert_eq!(cx.read(|cx| view.read(cx).draft.options.icon.clone()), None);

    set_icon(&view, &mut cx, &icon);
    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);

    click(&mut cx, "primary-action");
    cx.simulate_new_path_selection(|_| Some(destination.clone()));
    until(&mut cx, &view, "export", |view| !view.busy && (view.outcome.is_some() || view.error.is_some())).await;
    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);

    let unpacked = BundleArchive::unpack(&destination, ArchiveLimits::default(), Control::default()).expect("export");
    let bundle = unpacked.bundle_path();

    for dylib in ["Local.dylib", "Remote.dylib", "Substrate.dylib"] {
        assert!(bundle.join("Frameworks").join(dylib).is_file(), "{dylib} is injected");
    }

    for (file_name, size) in [("AppIcon.png", 20), ("AppIcon@2x.png", 40)] {
        let replaced = image::open(bundle.join(file_name)).expect("icon").to_rgba8();
        let Rgba([red, green, blue, _]) = *replaced.get_pixel(size / 2, size / 2);

        assert_eq!(replaced.dimensions(), (size, size), "{file_name} keeps its declared size");
        assert!(red > 150 && green < 90 && blue < 90, "{file_name} shows the custom icon");
    }
}
