use super::factor::{MAX_CODE_ATTEMPTS, MAX_SMS_REQUESTS};
use super::*;
use crate::wire::{self, dictionary};
use futures::FutureExt;
use plist::Value;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const USERNAME: &str = "fixture@example.test";
const PASSWORD: &str = "test password";

#[test]
fn restored_session_validates_fields_without_exposing_the_token() {
    let dsid = Zeroizing::new("123456789".into());
    let token = Zeroizing::new("private-fixture-token".into());
    let session = AuthSession::restore(USERNAME.into(), dsid, token, true).expect("restored session");

    assert_eq!(session.username(), USERNAME);
    assert!(session.using_alternate());
    assert!(!format!("{session:?}").contains("private-fixture-token"));

    let invalid = AuthSession::restore(
        USERNAME.into(),
        Zeroizing::new("123456789".into()),
        Zeroizing::new("bad\r\ntoken".into()),
        false,
    );

    assert!(matches!(&invalid, Err(Error::Invalid("authentication session"))));
    assert!(!invalid.expect_err("invalid token").to_string().contains("bad"));
}

#[derive(Debug)]
struct FixedProvider {
    name: &'static str,
    calls: AtomicUsize,
}

impl FixedProvider {
    fn new(name: &'static str) -> Self {
        Self { name, calls: AtomicUsize::new(0) }
    }
}

impl AnisetteProvider for FixedProvider {
    fn headers<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<AnisetteHeaders>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let values: BTreeMap<_, _> = [
            ("X-Apple-I-MD", "fixture-otp"),
            ("X-Apple-I-MD-M", "fixture-machine-token"),
            ("X-Mme-Device-Id", "fixture-device"),
            ("X-MMe-Client-Info", self.name),
            ("X-Apple-Locale", "en_US"),
            ("X-Apple-I-TimeZone", "UTC"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();

        async move { AnisetteHeaders::new(values) }.boxed()
    }
}

#[derive(Default)]
struct FixedDelegate {
    replies: Mutex<VecDeque<FactorReply>>,
    prompts: Mutex<Vec<FactorPrompt>>,
}

impl FixedDelegate {
    fn with(replies: impl IntoIterator<Item = FactorReply>) -> Self {
        Self { replies: Mutex::new(replies.into_iter().collect()), prompts: Mutex::new(Vec::new()) }
    }

    fn prompts(&self) -> Vec<FactorPrompt> {
        self.prompts.lock().expect("prompt history").clone()
    }
}

impl FactorDelegate for FixedDelegate {
    fn ask<'a>(&'a self, prompt: FactorPrompt) -> BoxFuture<'a, Result<FactorReply>> {
        self.prompts.lock().expect("prompt history").push(prompt);
        let reply = self.replies.lock().expect("replies").pop_front().expect("fixture reply");

        async move { Ok(reply) }.boxed()
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Scenario {
    alternate: bool,
    factor: bool,
    repeat_factor: bool,
    bad_proof: bool,
    bad_negotiation: bool,
    delay_init: bool,
    idmsdata: bool,
    custodian: bool,
    no_trusted_devices: bool,
    trusted_device_status: Option<u16>,
}

#[derive(Clone)]
struct Responder {
    vector: Arc<Json>,
    scenario: Scenario,
    completes: Arc<AtomicUsize>,
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match (request.method.as_str(), request.url.path()) {
            ("GET", "/auth") => {
                let mut response = phone();
                response["noTrustedDevices"] = Json::Bool(self.scenario.no_trusted_devices);

                ResponseTemplate::new(200).set_body_json(response)
            }
            ("GET", "/auth/verify/trusteddevice") => match self.scenario.trusted_device_status {
                Some(status) => ResponseTemplate::new(status).set_body_string("fixture rejection"),
                None => ResponseTemplate::new(200).set_body_json(phone()),
            },
            ("PUT", "/auth/verify/phone") => ResponseTemplate::new(200).set_body_json(phone()),
            ("POST", "/auth/verify/phone/securitycode") => {
                ResponseTemplate::new(200).set_body_json(json!({ "serviceErrors": [] }))
            }
            ("GET" | "POST", "/grandslam/GsService2/validate") => {
                let error = if request.headers.get("security-code").is_some_and(|value| value == "000000") {
                    -21669
                } else {
                    0
                };

                plist_response(dictionary([("ec", Value::Integer(error.into()))]))
            }
            ("POST", "/grandslam/GsService2") => self.gsa(request),
            _ => ResponseTemplate::new(404),
        }
    }
}

impl Responder {
    fn gsa(&self, request: &Request) -> ResponseTemplate {
        let body = Value::from_reader_xml(request.body.as_slice()).expect("GSA request plist");
        let operation = field(field(&body, "Request"), "o").as_string().expect("GSA operation");

        let response = match operation {
            "init" => dictionary([
                ("sp", Value::String("s2k".into())),
                ("i", Value::Integer(1000.into())),
                ("s", Value::Data(binary(&self.vector, "salt"))),
                ("B", Value::Data(binary(&self.vector, "server_public"))),
                ("c", Value::Data(b"fixture-init-continuation".to_vec())),
            ]),
            "complete" => {
                let attempt = self.completes.fetch_add(1, Ordering::SeqCst);
                let primary = request.headers.get("x-mme-client-info").is_some_and(|value| value == "primary-client");

                if self.scenario.alternate && primary {
                    return plist_response(envelope(dictionary([]), -36607, None));
                }

                let factor = self.scenario.factor && (attempt == 0 || self.scenario.repeat_factor);
                let (session_key, negotiation_key) = match (factor, self.scenario.idmsdata, self.scenario.custodian) {
                    (true, true, _) => ("encrypted_session_with_idmsdata", "negotiation_with_idmsdata"),
                    (true, false, true) => ("encrypted_session", "negotiation"),
                    (true, false, false) => ("encrypted_session_no_custodian", "negotiation_no_custodian"),
                    (false, _, _) => ("encrypted_session", "negotiation"),
                };
                let mut server_proof = binary(&self.vector, "server_proof");
                let mut negotiation = binary(&self.vector, negotiation_key);

                if self.scenario.bad_proof {
                    server_proof[0] ^= 1;
                }

                if self.scenario.bad_negotiation {
                    negotiation[0] ^= 1;
                }

                dictionary([
                    ("M2", Value::Data(server_proof)),
                    ("spd", Value::Data(binary(&self.vector, session_key))),
                    ("np", Value::Data(negotiation)),
                    ("sc", Value::Data(binary(&self.vector, "context"))),
                ])
            }
            "apptokens" => dictionary([("et", Value::Data(binary(&self.vector, "token")))]),
            _ => panic!("unexpected GSA operation"),
        };

        let unlock = (operation == "complete"
            && self.scenario.factor
            && (self.completes.load(Ordering::SeqCst) == 1 || self.scenario.repeat_factor))
            .then_some("trustedDevice");

        let mut response = plist_response(envelope(response, 0, unlock))
            .insert_header("Set-Cookie", "fixture-session=active; Path=/; HttpOnly");

        if operation == "init" && self.scenario.delay_init {
            response = response.set_delay(std::time::Duration::from_secs(2));
        }

        response
    }
}

struct Harness {
    server: MockServer,
    client: AuthClient,
    primary: Arc<FixedProvider>,
    alternate: Arc<FixedProvider>,
    completes: Arc<AtomicUsize>,
}

impl Harness {
    async fn new(scenario: Scenario) -> Self {
        let server = MockServer::start().await;
        let vector = Arc::new(vector());
        let completes = Arc::new(AtomicUsize::new(0));
        let responder = Responder { vector: vector.clone(), scenario, completes: completes.clone() };

        Mock::given(method("GET")).respond_with(responder.clone()).mount(&server).await;
        Mock::given(method("POST")).respond_with(responder).mount(&server).await;

        let mut client = AuthClient::with_origin(&server.uri()).expect("fixture origin");
        client.ephemeral = Some(binary(&vector, "ephemeral").try_into().expect("32-byte secret"));

        Self {
            server,
            client,
            primary: Arc::new(FixedProvider::new("primary-client")),
            alternate: Arc::new(FixedProvider::new("alternate-client")),
            completes,
        }
    }

    fn sources(&self) -> AnisetteSources {
        AnisetteSources::new(self.primary.clone()).with_alternate(self.alternate.clone())
    }

    async fn login(&self, delegate: &dyn FactorDelegate) -> Result<AuthSession> {
        self.client
            .login(
                USERNAME.into(),
                Zeroizing::new(PASSWORD.into()),
                &self.sources(),
                delegate,
                &CancellationToken::new(),
            )
            .await
    }

    async fn requests(&self) -> Vec<Request> {
        self.server.received_requests().await.expect("received requests")
    }
}

fn vector() -> Json {
    let fixture: Json =
        serde_json::from_str(include_str!("../../tests/fixtures/grandslam.json")).expect("independent fixture JSON");

    fixture["vectors"][0].clone()
}

fn binary(vector: &Json, field: &str) -> Vec<u8> {
    hex::decode(vector[field].as_str().expect("hex fixture field")).expect("fixture hex")
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    wire::dict(value).expect("dictionary").get(name).expect("fixture field")
}

fn phone() -> Json {
    json!({
        "otherTrustedDeviceClass": "iPhone",
        "maskedPhoneNumber": "***1234",
        "securityCode": { "length": 6 },
        "trustedPhoneNumber": { "id": 7, "obfuscatedNumber": "***1234" },
        "trustedPhoneNumbers": [{ "id": 7, "numberWithDialCode": "***1234" }],
    })
}

fn envelope(response: Value, code: i64, unlock: Option<&str>) -> Value {
    let mut status =
        wire::dict(&dictionary([("ec", Value::Integer(code.into())), ("em", Value::String("fixture status".into()))]))
            .expect("fixture status")
            .clone();

    if let Some(unlock) = unlock {
        status.insert("au".into(), Value::String(unlock.into()));
    }

    let mut response = wire::dict(&response).expect("fixture response").clone();
    response.insert("Status".into(), Value::Dictionary(status));

    dictionary([("Response", Value::Dictionary(response))])
}

fn plist_response(value: Value) -> ResponseTemplate {
    let mut body = Vec::new();
    value.to_writer_xml(&mut body).expect("fixture plist");

    ResponseTemplate::new(200).set_body_bytes(body)
}

#[tokio::test]
async fn gsa_exchange_matches_wire_contract_and_cookie_scoping() {
    let harness = Harness::new(Scenario::default()).await;
    let session = harness.login(&FixedDelegate::default()).await.expect("authenticated session");

    assert_eq!(session.username(), USERNAME);
    assert_eq!(session.dsid(), "123456789");
    assert_eq!(session.token(), "fixture-xcode-token");
    assert!(!session.using_alternate());
    assert!(!format!("{session:?}").contains("fixture-xcode-token"));

    let requests = harness.requests().await;
    let operations: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/grandslam/GsService2")
        .map(|request| Value::from_reader_xml(request.body.as_slice()).expect("request"))
        .collect();
    assert_eq!(operations.len(), 3);

    for (index, operation) in operations.iter().enumerate() {
        let request = field(operation, "Request");
        let cpd = wire::dict(field(request, "cpd")).expect("client-provided data");

        assert_eq!(field(field(operation, "Header"), "Version").as_string(), Some("1.0.1"));
        assert_eq!(field(request, "o").as_string(), Some(["init", "complete", "apptokens"][index]));
        assert_eq!(cpd.get("loc").and_then(Value::as_string), Some("en_US"));
        assert!(!cpd.contains_key("X-Apple-Locale"));
        assert_eq!(field(request, "AppleIDClientIdentifier").as_string(), Some("fixture-device"));
        assert_eq!(field(request, "svct").as_string(), Some("iCloud"));
        assert_eq!(field(request, "bootstrap").as_boolean(), Some(true));
        assert_eq!(field(request, "pbe").as_boolean(), Some(true));
        assert_eq!(field(request, "wpbe").as_boolean(), Some(true));
        assert_eq!(field(request, "X-Apple-I-Device-Configuration-Mode").as_string(), Some("0"));
    }

    let init = field(&operations[0], "Request");
    let complete = field(&operations[1], "Request");
    let tokens = field(&operations[2], "Request");
    let vector = vector();

    assert_eq!(field(init, "A2k").as_data(), Some(binary(&vector, "public").as_slice()));
    assert_eq!(field(complete, "M1").as_data(), Some(binary(&vector, "client_proof").as_slice()));
    assert_eq!(field(tokens, "u").as_string(), Some("123456789"));
    assert_eq!(field(tokens, "checksum").as_data(), Some(binary(&vector, "checksum").as_slice()));
    assert_eq!(field(field(init, "cpd"), "prkgen").as_boolean(), Some(true));
    assert_eq!(field(field(complete, "cpd"), "ckgen").as_boolean(), Some(true));
    assert!(!String::from_utf8_lossy(&requests[0].body).contains(PASSWORD));
    assert_eq!(requests[1].headers.get("cookie").expect("cookie"), "fixture-session=active");

    for request in &requests {
        assert_eq!(request.headers.get("user-agent").expect("user agent"), "Xcode");
        assert_eq!(request.headers.get("x-mme-client-info").expect("client info"), "primary-client");
    }
}

#[tokio::test]
async fn complete_mismatch_switches_provider_and_restarts_once() {
    let harness = Harness::new(Scenario { alternate: true, ..Default::default() }).await;
    let session = harness.login(&FixedDelegate::default()).await.expect("alternate session");

    assert!(session.using_alternate());
    assert_eq!(harness.completes.load(Ordering::SeqCst), 2);
    assert_eq!(harness.primary.calls.load(Ordering::SeqCst), 2);
    assert_eq!(harness.alternate.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn trusted_device_code_and_login_restart_complete() {
    let harness = Harness::new(Scenario { factor: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([FactorReply::Code(Zeroizing::new("123456".into()))]);
    let session = harness.login(&delegate).await.expect("second-factor session");

    assert_eq!(session.token(), "fixture-xcode-token");
    assert_eq!(harness.completes.load(Ordering::SeqCst), 2);
    assert_eq!(delegate.prompts()[0].destination, "iPhone");
    assert_eq!(delegate.prompts()[0].code_length, 6);

    let requests = harness.requests().await;
    let validation = requests.iter().find(|request| request.url.path().ends_with("/validate")).expect("validation");
    assert_eq!(validation.headers.get("security-code").expect("verification code"), "123456");
    assert_eq!(validation.headers.get("cookie").expect("shared cookie"), "fixture-session=active");
    assert!(validation.headers.get("x-apple-identity-token").is_some());
    assert!(requests.iter().any(|request| request.url.path() == "/auth/verify/trusteddevice"));
}

#[tokio::test]
async fn idmsdata_uses_post_validation_with_authenticated_body() {
    let harness = Harness::new(Scenario { factor: true, idmsdata: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([FactorReply::Code(Zeroizing::new("123456".into()))]);
    harness.login(&delegate).await.expect("second-factor session");

    let requests = harness.requests().await;
    let validation = requests.iter().find(|request| request.url.path().ends_with("/validate")).expect("validation");
    let body = Value::from_reader_xml(validation.body.as_slice()).expect("validation plist");

    assert_eq!(validation.method.as_str(), "POST");
    assert_eq!(field(field(&body, "Request"), "idmsdata").as_data(), Some(b"fixture-idmsdata".as_slice()));
}

#[tokio::test]
async fn incorrect_code_reprompts_and_repeated_factor_is_rejected() {
    let harness = Harness::new(Scenario { factor: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([
        FactorReply::Code(Zeroizing::new("000000".into())),
        FactorReply::Code(Zeroizing::new("123456".into())),
    ]);
    harness.login(&delegate).await.expect("second code succeeds");

    assert_eq!(delegate.prompts().len(), 2);
    assert!(delegate.prompts()[1].incorrect_code);

    let repeated = Harness::new(Scenario { factor: true, repeat_factor: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([FactorReply::Code(Zeroizing::new("123456".into()))]);
    assert!(matches!(repeated.login(&delegate).await, Err(Error::RetryLimit("second-factor login restart"))));
    assert_eq!(repeated.completes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sms_request_and_trusted_device_fallback_use_phone_path() {
    for status in [None, Some(401)] {
        let harness =
            Harness::new(Scenario { factor: true, trusted_device_status: status, ..Default::default() }).await;
        let replies = if status.is_none() {
            vec![FactorReply::RequestSms, FactorReply::Code(Zeroizing::new("123456".into()))]
        } else {
            vec![FactorReply::Code(Zeroizing::new("123456".into()))]
        };
        let delegate = FixedDelegate::with(replies);
        harness.login(&delegate).await.expect("SMS login");

        let requests = harness.requests().await;
        let delivery = requests
            .iter()
            .find(|request| request.method.as_str() == "PUT" && request.url.path() == "/auth/verify/phone")
            .expect("SMS delivery request");
        let body: Json = serde_json::from_slice(&delivery.body).expect("SMS delivery JSON");

        assert_eq!(body, json!({ "phoneNumber": { "id": 7 }, "mode": "sms" }));
        assert!(requests.iter().any(|request| request.url.path() == "/auth/verify/phone/securitycode"));
        assert!(!requests.iter().any(|request| request.url.path().ends_with("/validate")));
        assert_eq!(delegate.prompts().last().expect("SMS prompt").destination, "***1234");
    }
}

#[tokio::test]
async fn invalid_proofs_do_not_request_app_tokens() {
    for scenario in
        [Scenario { bad_proof: true, ..Default::default() }, Scenario { bad_negotiation: true, ..Default::default() }]
    {
        let harness = Harness::new(scenario).await;
        assert!(matches!(harness.login(&FixedDelegate::default()).await, Err(Error::Verification(_))));
        assert_eq!(
            harness.requests().await.iter().filter(|request| request.url.path() == "/grandslam/GsService2").count(),
            2
        );
    }
}

#[tokio::test]
async fn factor_attempt_limits_bound_requests() {
    let invalid_codes = Harness::new(Scenario { factor: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with((0..MAX_CODE_ATTEMPTS).map(|_| FactorReply::Code(Zeroizing::new("bad".into()))));

    assert!(matches!(invalid_codes.login(&delegate).await, Err(Error::RetryLimit("verification code attempts"))));
    assert_eq!(delegate.prompts().len(), MAX_CODE_ATTEMPTS);
    assert!(!invalid_codes.requests().await.iter().any(|request| request.url.path().ends_with("/validate")));

    let repeated_sms = Harness::new(Scenario { factor: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with((0..=MAX_SMS_REQUESTS).map(|_| FactorReply::RequestSms));

    assert!(matches!(repeated_sms.login(&delegate).await, Err(Error::RetryLimit("SMS requests"))));
    assert!(!delegate.prompts().last().expect("final prompt").can_request_sms);
    assert_eq!(
        repeated_sms.requests().await.iter().filter(|request| request.url.path() == "/auth/verify/phone").count(),
        MAX_SMS_REQUESTS
    );
}

#[tokio::test]
async fn custodian_session_requests_trusted_device_delivery() {
    let harness = Harness::new(Scenario { factor: true, custodian: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([FactorReply::Code(Zeroizing::new("123456".into()))]);
    harness.login(&delegate).await.expect("second-factor session");

    let requests = harness.requests().await;

    assert!(requests.iter().any(|request| request.url.path() == "/auth"));
    assert!(requests.iter().any(|request| request.url.path() == "/auth/verify/trusteddevice"));
}

#[tokio::test]
async fn no_trusted_devices_uses_automatically_delivered_phone_code() {
    let harness = Harness::new(Scenario { factor: true, no_trusted_devices: true, ..Default::default() }).await;
    let delegate = FixedDelegate::with([FactorReply::Code(Zeroizing::new("123456".into()))]);
    harness.login(&delegate).await.expect("phone-code session");

    let requests = harness.requests().await;

    assert!(!requests.iter().any(|request| request.url.path() == "/auth/verify/trusteddevice"));
    assert!(!requests.iter().any(|request| request.url.path() == "/auth/verify/phone"));
    assert!(requests.iter().any(|request| request.url.path() == "/auth/verify/phone/securitycode"));
}

#[tokio::test]
async fn cancellation_during_init_prevents_completion() {
    let harness = Harness::new(Scenario { delay_init: true, ..Default::default() }).await;
    let cancellation = CancellationToken::new();
    let delegate = FixedDelegate::default();
    let sources = harness.sources();

    let login =
        harness.client.login(USERNAME.into(), Zeroizing::new(PASSWORD.into()), &sources, &delegate, &cancellation);
    let cancel = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(login, cancel);

    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!harness.requests().await.iter().any(|request| request.url.path().ends_with("/validate")));
    assert_eq!(harness.completes.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_authentication_origins_are_rejected() {
    for origin in [
        "http://example.test",
        "http://localhost",
        "https://user:secret@example.test",
        "https://example.test/path",
        "file:///tmp",
        "http://127.0.0.1/#fragment",
    ] {
        assert!(AuthClient::with_origin(origin).is_err());
    }
}
