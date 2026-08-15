//! `URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder` — the web globals
//! the npm ecosystem assumes exist. Engine-provided so every function gets
//! them without importing a polyfill.

use rusted_engine::{Executor, HttpRequest, Limits, Outcome, QuickJsExecutor};

fn run(body: &str) -> String {
    let src = format!("export default async function handler(request, context) {{\n{body}\n}}");
    let r = QuickJsExecutor::new().execute(&src, &HttpRequest::post_json("{}"), &Limits::default());
    match r.outcome {
        Outcome::Success(s) => s,
        o => panic!("expected Success, got {o:?}"),
    }
}

#[test]
fn url_parses_into_its_components() {
    let out = run(r#"
        const u = new URL("https://user:pw@example.com:8443/a/b%20c?x=1&y=two#frag");
        return context.json({
            href: u.href, protocol: u.protocol, username: u.username,
            password: u.password, host: u.host, hostname: u.hostname,
            port: u.port, pathname: u.pathname, search: u.search,
            hash: u.hash, origin: u.origin,
        });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["protocol"], "https:");
    assert_eq!(v["username"], "user");
    assert_eq!(v["password"], "pw");
    assert_eq!(v["host"], "example.com:8443");
    assert_eq!(v["hostname"], "example.com");
    assert_eq!(v["port"], "8443");
    assert_eq!(v["pathname"], "/a/b%20c");
    assert_eq!(v["search"], "?x=1&y=two");
    assert_eq!(v["hash"], "#frag");
    assert_eq!(v["origin"], "https://example.com:8443");
}

#[test]
fn url_default_port_is_empty_and_omitted_from_host() {
    let out = run(r#"
        const u = new URL("https://example.com:443/path");
        return context.json({ port: u.port, host: u.host, href: u.href });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["port"], "");
    assert_eq!(v["host"], "example.com");
    assert_eq!(v["href"], "https://example.com/path");
}

#[test]
fn url_resolves_relative_references_against_a_base() {
    let out = run(r#"
        const u = new URL("../c?d=1", "https://example.com/a/b/");
        return u.href;
    "#);
    assert_eq!(out, "https://example.com/a/c?d=1");
}

#[test]
fn invalid_url_throws_a_catchable_error() {
    let out = run(r#"
        try { new URL("not a url"); return "no throw"; }
        catch (e) { return `caught ${e instanceof TypeError ? "TypeError" : "other"}`; }
    "#);
    assert_eq!(out, "caught TypeError");
}

#[test]
fn search_params_read_write_and_serialize() {
    let out = run(r#"
        const p = new URLSearchParams("a=1&b=two%20words&a=3");
        const before = { a: p.get("a"), all: p.getAll("a"), b: p.get("b"), has: p.has("c") };
        p.set("b", "changed");
        p.append("c", "new & shiny");
        p.delete("a");
        return context.json({ before, after: p.toString() });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["before"]["a"], "1");
    assert_eq!(v["before"]["all"], serde_json::json!(["1", "3"]));
    assert_eq!(v["before"]["b"], "two words");
    assert_eq!(v["before"]["has"], false);
    assert_eq!(v["after"], "b=changed&c=new+%26+shiny");
}

#[test]
fn search_params_iterate_and_construct_from_objects() {
    let out = run(r#"
        const fromObject = new URLSearchParams({ x: "1", y: "2" });
        const fromPairs = new URLSearchParams([["k", "v"], ["k", "w"]]);
        const seen = [];
        for (const [key, value] of fromPairs) seen.push(`${key}=${value}`);
        return context.json({
            object: fromObject.toString(),
            pairs: seen,
            spread: [...fromObject.keys()],
        });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["object"], "x=1&y=2");
    assert_eq!(v["pairs"], serde_json::json!(["k=v", "k=w"]));
    assert_eq!(v["spread"], serde_json::json!(["x", "y"]));
}

#[test]
fn url_search_params_are_live_linked_to_the_url() {
    let out = run(r#"
        const u = new URL("https://example.com/p?a=1");
        u.searchParams.set("a", "2");
        u.searchParams.append("b", "3");
        const forward = u.href;
        u.search = "?fresh=yes";
        return context.json({ forward, back: u.searchParams.get("fresh") });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["forward"], "https://example.com/p?a=2&b=3");
    assert_eq!(v["back"], "yes");
}

#[test]
fn url_component_setters_reserialize() {
    let out = run(r#"
        const u = new URL("https://example.com/old?q=1#h");
        u.pathname = "/new path";
        u.hash = "there";
        u.port = "8080";
        return u.href;
    "#);
    assert_eq!(out, "https://example.com:8080/new%20path?q=1#there");
}

#[test]
fn text_encoder_round_trips_multibyte_utf8() {
    let out = run(r#"
        const bytes = new TextEncoder().encode("héllo → ok");
        const text = new TextDecoder().decode(bytes);
        return context.json({
            isU8: bytes instanceof Uint8Array,
            len: bytes.byteLength,
            text,
            encoding: new TextDecoder().encoding,
        });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["isU8"], true);
    assert_eq!(v["len"], 13); // é=2, →=3 bytes over the 8 ASCII chars
    assert_eq!(v["text"], "héllo → ok");
    assert_eq!(v["encoding"], "utf-8");
}

#[test]
fn text_decoder_replaces_by_default_and_throws_when_fatal() {
    let out = run(r#"
        const bad = new Uint8Array([104, 105, 0xFF]);
        const lossy = new TextDecoder().decode(bad);
        let fatal = "no throw";
        try { new TextDecoder("utf-8", { fatal: true }).decode(bad); }
        catch (e) { fatal = e instanceof TypeError ? "TypeError" : "other"; }
        return context.json({ lossy, fatal });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["lossy"], "hi\u{FFFD}");
    assert_eq!(v["fatal"], "TypeError");
}

#[test]
fn text_decoder_accepts_array_buffers_and_rejects_unknown_labels() {
    let out = run(r#"
        const buf = new TextEncoder().encode("buffer").buffer;
        const text = new TextDecoder("utf-8").decode(buf);
        let label = "no throw";
        try { new TextDecoder("shift-jis"); }
        catch (e) { label = e instanceof RangeError ? "RangeError" : "other"; }
        return context.json({ text, label, empty: new TextDecoder().decode() });
    "#);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["text"], "buffer");
    assert_eq!(v["label"], "RangeError");
    assert_eq!(v["empty"], "");
}
