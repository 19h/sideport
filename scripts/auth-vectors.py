#!/usr/bin/env python3
"""Generate public test credentials with pinned PySRP and PyCryptodome.

Usage: python3 scripts/auth-vectors.py <pysrp.py> <output.json>
Reference: https://raw.githubusercontent.com/cocagne/pysrp/1.0.22/srp/_pysrp.py
The reference implementation is an independent oracle, never a runtime dependency.
"""

import hashlib
import hmac
import importlib.util
import json
import pathlib
import plistlib
import sys

from Crypto.Cipher import AES
from Crypto.Util.Padding import pad


REFERENCE_SHA256 = "a1600e7ee7b2b7b203f9cad488fd7e0a441297597743ba5433ed9c5193e7a359"
APP = "com.apple.gs.xcode.auth"


def load_reference(path):
    source = path.read_bytes()

    if hashlib.sha256(source).hexdigest() != REFERENCE_SHA256:
        raise ValueError("PySRP source does not match the pinned reference")

    spec = importlib.util.spec_from_file_location("reference_srp", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.rfc5054_enable()
    module.no_username_in_x()

    return module


def fragment(value):
    xml = plistlib.dumps(value)
    start = xml.index(b"<dict>")
    end = xml.rindex(b"</dict>") + len(b"</dict>")

    return xml[start:end]


def mac(key, message):
    return hmac.digest(key, message, "sha256")


def session_ciphertext(data_key, data_iv, fields):
    encoded = fragment(fields)

    return AES.new(data_key, AES.MODE_CBC, data_iv).encrypt(pad(encoded, 16))


def negotiation_proof(session_key, protocol, ciphertext, context):
    transcript = b"s2k,s2k_fo" + protocol.encode() + b"|" + ciphertext + b"|" + context
    digest = hashlib.sha256(transcript).digest()

    return mac(mac(session_key, b"HMAC key:"), digest)


def exchange(
    srp,
    name,
    protocol,
    a,
    b,
    salt,
    password="test password",
    username="fixture@example.test",
    key_length=32,
):
    password_hash = hashlib.sha256(password.encode()).digest()
    input_key = password_hash.hex().encode() if protocol == "s2k_fo" else password_hash
    derived = hashlib.pbkdf2_hmac("sha256", input_key, salt, 1000, 32)
    modulus, generator = srp.get_ng(srp.NG_2048, None, None)
    private = srp.gen_x(hashlib.sha256, salt, username, derived)
    verifier = pow(generator, private, modulus)

    client = srp.User(username, derived, hash_alg=srp.SHA256, bytes_a=a.to_bytes(256, "big"))
    _, public = client.start_authentication()
    server = srp.Verifier(
        username,
        salt,
        srp.long_to_bytes(verifier),
        public,
        hash_alg=srp.SHA256,
        bytes_b=b.to_bytes(256, "big"),
    )
    _, server_public = server.get_challenge()
    proof = client.process_challenge(salt, server_public)
    server_proof = server.verify_session(proof)
    client.verify_session(server_proof)
    session_key = client.get_session_key()

    assert client.authenticated()
    assert session_key == server.get_session_key()

    app_key = bytes(range(key_length))
    session_fields = {
        "adsid": "123456789",
        "GsIdmsToken": "fixture-idms-token",
        "sk": app_key,
        "c": b"fixture-continuation",
        "sm": {"fixture": "unlock-data"},
    }
    data_key = mac(session_key, b"extra data key:")
    data_iv = mac(session_key, b"extra data iv:")[:16]
    context = b"fixture-context"

    encrypted_session = session_ciphertext(data_key, data_iv, {**session_fields, "canHaveCustodian": True})
    encrypted_session_no_custodian = session_ciphertext(data_key, data_iv, session_fields)
    encrypted_session_with_idmsdata = session_ciphertext(
        data_key, data_iv, {**session_fields, "idmsdata": b"fixture-idmsdata"}
    )

    negotiation = negotiation_proof(session_key, protocol, encrypted_session, context)
    negotiation_no_custodian = negotiation_proof(
        session_key, protocol, encrypted_session_no_custodian, context
    )
    negotiation_with_idmsdata = negotiation_proof(
        session_key, protocol, encrypted_session_with_idmsdata, context
    )

    token = fragment({"t": {APP: {"token": "fixture-xcode-token"}}})
    nonce = bytes(range(16))
    cipher = AES.new(app_key, AES.MODE_GCM, nonce=nonce)
    cipher.update(b"XYZ")
    ciphertext, tag = cipher.encrypt_and_digest(token)
    envelope = b"XYZ" + nonce + ciphertext + tag

    vector = {
        "name": name,
        "protocol": protocol,
        "username": username,
        "password": password,
        "iterations": 1000,
        "ephemeral": a.to_bytes(32, "big").hex(),
        "salt": salt.hex(),
        "public": public.hex(),
        "server_public": server_public.hex(),
        "client_proof": proof.hex(),
        "server_proof": server_proof.hex(),
        "key": session_key.hex(),
        "encrypted_session": encrypted_session.hex(),
        "context": context.hex(),
        "negotiation": negotiation.hex(),
        "encrypted_session_no_custodian": encrypted_session_no_custodian.hex(),
        "negotiation_no_custodian": negotiation_no_custodian.hex(),
        "encrypted_session_with_idmsdata": encrypted_session_with_idmsdata.hex(),
        "negotiation_with_idmsdata": negotiation_with_idmsdata.hex(),
        "token": envelope.hex(),
        "checksum": mac(app_key, b"apptokens123456789" + APP.encode()).hex(),
    }

    return vector, client.S


def main():
    reference_path, output_path = map(pathlib.Path, sys.argv[1:])
    srp = load_reference(reference_path)
    a = int("60975527035cf2ad1989806f0407210bc81edc04e2762a56afd529ddda2d4393", 16)
    b = int("e487cb59d31ac550471e81f00f6928e01dda08e974a004f49e61f5d105284d20", 16)
    salt = bytes.fromhex("beb25379d1a8581eb5a727673a2441ee")

    cases = [
        ("s2k", "s2k", a, b, salt, "test password", "fixture@example.test"),
        ("s2k_fo", "s2k_fo", a, b, salt, "test password", "fixture@example.test"),
        ("minimal-public", "s2k", 1, b, salt, "test password", "fixture@example.test"),
        ("leading-zero-salt-utf8", "s2k_fo", a, b, b"\x00\x00" + salt, "päss🔬", "Ελένη@example.test"),
        ("empty-password", "s2k", a, b, salt, "", "fixture@example.test"),
        ("aes128-token", "s2k", a, b, salt, "test password", "fixture@example.test", 16),
        ("aes192-token", "s2k", a, b, salt, "test password", "fixture@example.test", 24),
    ]
    vectors = [exchange(srp, *case)[0] for case in cases]

    for counter in range(1, 4096):
        vector, shared = exchange(srp, "minimal-shared-secret", "s2k", a, counter, salt)

        if shared.bit_length() <= 2040:
            vectors.append(vector)
            break
    else:
        raise AssertionError("failed to find a short shared secret")

    output_path.write_text(json.dumps({"reference_sha256": REFERENCE_SHA256, "vectors": vectors}, indent=2) + "\n")


if __name__ == "__main__":
    main()
