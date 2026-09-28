//! AOSKit one-time-password headers (recovered `kbsync.privateAnisetter`).
//!
//! `init()` loads `/System/Library/PrivateFrameworks/AOSKit.framework` and requires the class
//! `AOSUtilities` with `retrieveOTPHeadersForDSID:`, `machineSerialNumber` and `machineUDID`
//! (recovered "AOS incompatible"). `aos_do()` asks for DSID `-2` and reads `X-Apple-MD` and
//! `X-Apple-MD-M`, then adds the locale language code and time-zone abbreviation.

use crate::{Error, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSBundle, NSLocale, NSString, NSTimeZone};
use zeroize::Zeroizing;

const FRAMEWORK: &str = "/System/Library/PrivateFrameworks/AOSKit.framework";

pub(crate) struct OtpHeaders {
    pub otp: Zeroizing<String>,
    pub machine_token: Zeroizing<String>,
    pub serial: String,
    pub device_id: String,
    pub locale: String,
    pub time_zone: String,
}

fn utilities() -> Result<&'static AnyClass> {
    let path = NSString::from_str(FRAMEWORK);
    let bundle = NSBundle::bundleWithPath(&path).ok_or(Error::AosIncompatible("AOSKit.framework"))?;

    // SAFETY: loading a system framework bundle runs its initializers, as the recovered client
    // does; the path is fixed to Apple's private AOSKit framework.
    if !unsafe { bundle.load() } {
        return Err(Error::AosIncompatible("AOSKit.framework could not be loaded"));
    }

    let class = AnyClass::get(c"AOSUtilities").ok_or(Error::AosIncompatible("AOSUtilities"))?;
    let required: [Sel; 3] = [sel!(retrieveOTPHeadersForDSID:), sel!(machineSerialNumber), sel!(machineUDID)];

    if !required.iter().all(|selector| class.metaclass().responds_to(*selector)) {
        return Err(Error::AosIncompatible("AOSUtilities methods"));
    }

    Ok(class)
}

fn string(value: Option<Retained<AnyObject>>) -> Option<String> {
    let value = value?.downcast::<NSString>().ok()?;
    let text = value.to_string();

    (!text.is_empty()).then_some(text)
}

pub(crate) fn otp_headers() -> Result<OtpHeaders> {
    let class = utilities()?;
    let dsid = NSString::from_str("-2");

    // SAFETY: the selectors were checked above; each returns an autoreleased object or nil,
    // which `Option<Retained<_>>` retains or maps to `None`.
    let headers: Option<Retained<AnyObject>> = unsafe { msg_send![class, retrieveOTPHeadersForDSID: &*dsid] };
    let serial: Option<Retained<AnyObject>> = unsafe { msg_send![class, machineSerialNumber] };
    let device_id: Option<Retained<AnyObject>> = unsafe { msg_send![class, machineUDID] };

    let headers = headers.ok_or(Error::AosFailed("OTP headers"))?;

    let header = |name: &str| {
        let key = NSString::from_str(name);

        // SAFETY: `objectForKey:` on an NSDictionary returns the value or nil.
        let value: Option<Retained<AnyObject>> = unsafe { msg_send![&*headers, objectForKey: &*key] };

        string(value)
    };

    let otp = header("X-Apple-MD").ok_or(Error::AosFailed("X-Apple-MD"))?;
    let machine_token = header("X-Apple-MD-M").ok_or(Error::AosFailed("X-Apple-MD-M"))?;

    let locale = NSLocale::currentLocale();
    let language = locale.languageCode().to_string();
    let time_zone = NSTimeZone::localTimeZone().abbreviation().map(|abbreviation| abbreviation.to_string());

    Ok(OtpHeaders {
        otp: Zeroizing::new(otp),
        machine_token: Zeroizing::new(machine_token),
        serial: string(serial).ok_or(Error::AosFailed("machineSerialNumber"))?,
        device_id: string(device_id).ok_or(Error::AosFailed("machineUDID"))?,
        locale: language,
        time_zone: time_zone.unwrap_or_else(|| "UTC".into()),
    })
}

/// Key names returned by AOSKit for diagnostics; values are not exposed.
pub(crate) fn otp_header_names() -> Result<Vec<String>> {
    let class = utilities()?;
    let dsid = NSString::from_str("-2");

    // SAFETY: as in `otp_headers`.
    let headers: Option<Retained<AnyObject>> = unsafe { msg_send![class, retrieveOTPHeadersForDSID: &*dsid] };
    let headers = headers.ok_or(Error::AosFailed("OTP headers"))?;

    // SAFETY: `allKeys` on an NSDictionary returns an NSArray of its keys.
    let keys: Retained<objc2_foundation::NSArray<AnyObject>> = unsafe { msg_send![&*headers, allKeys] };

    Ok(keys.iter().filter_map(|key| key.downcast::<NSString>().ok().map(|key| key.to_string())).collect())
}
