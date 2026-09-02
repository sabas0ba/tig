//! ブラウザ向け frontend。
//!
//! wasm-bindgen 等の glue crate に依存せず、C ABI と手書きの JS glue
//! (web/app.js) で接続する。バイト列の受け渡しは次の規約で行う。
//!
//! - JS → wasm: `tig_alloc(len)` で確保した領域へ書き込み、(ptr, len) を渡す。
//!   呼び出し後に `tig_dealloc(ptr, len)` で解放する
//! - wasm → JS: 返り値は「u32 (LE) の長さ + データ」のバッファへのポインタ。
//!   読み取り後に `tig_free(ptr)` で解放する。失敗時は null を返し、
//!   `tig_last_error()` が UTF-8 のメッセージを返す
//!
//! HTTP は持たない (sans-io)。clone / push は JS 側が `fetch()` で request を運ぶ。

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use tig_core::bundle::{self, Bundle};
use tig_core::clone::{Clone as CloneDriver, CloneOptions, Request};
use tig_core::history::Walk;
use tig_core::object::Kind;
use tig_core::oid::Oid;
use tig_core::pack;
use tig_core::push::{self, Push as PushDriver};

// --- バッファの受け渡し -----------------------------------------------------

/// JS が入力用に確保する領域。
#[unsafe(no_mangle)]
pub extern "C" fn tig_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

/// `tig_alloc` で確保した領域の解放。
///
/// # Safety
/// `ptr` は同じ `len` で `tig_alloc` が返した値であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

/// 長さ前置の出力バッファを作る。
fn out_bytes(payload: &[u8]) -> *mut u8 {
    let mut buf = Vec::with_capacity(payload.len() + 4);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    Box::into_raw(buf.into_boxed_slice()) as *mut u8
}

/// 出力バッファ (長さ前置) の解放。
///
/// # Safety
/// `ptr` は本ライブラリの API が返した出力バッファであること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_free(ptr: *mut u8) {
    let len_bytes = unsafe { std::slice::from_raw_parts(ptr, 4) };
    let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize + 4;
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
    static BUNDLES: RefCell<Vec<Option<OwnedBundle>>> = const { RefCell::new(Vec::new()) };
    static CLONES: RefCell<Vec<Option<CloneSlot>>> = const { RefCell::new(Vec::new()) };
    static PUSHES: RefCell<Vec<Option<PushSlot>>> = const { RefCell::new(Vec::new()) };
}

fn fail<T: Default>(message: impl Into<String>) -> T {
    LAST_ERROR.with(|e| *e.borrow_mut() = message.into());
    T::default()
}

/// 直近の失敗のメッセージ (UTF-8)。
#[unsafe(no_mangle)]
pub extern "C" fn tig_last_error() -> *mut u8 {
    LAST_ERROR.with(|e| out_bytes(e.borrow().as_bytes()))
}

// --- bundle -----------------------------------------------------------------

/// bundle のバイト列を leak した領域と、それを借りる Bundle を束ねる。
/// Bundle<'static> は data を参照するため、解放は close 時に順序を守って行う。
struct OwnedBundle {
    ptr: *mut u8,
    len: usize,
    bundle: ManuallyDrop<Bundle<'static>>,
}

impl Drop for OwnedBundle {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.bundle);
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.ptr, self.len,
            )));
        }
    }
}

fn slot_insert<T>(cell: &'static std::thread::LocalKey<RefCell<Vec<Option<T>>>>, item: T) -> i32 {
    cell.with(|slots| {
        let mut slots = slots.borrow_mut();
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(item);
                return i as i32;
            }
        }
        slots.push(Some(item));
        (slots.len() - 1) as i32
    })
}

/// bundle を解析して handle (>= 0) を返す。失敗時は -1。
///
/// # Safety
/// `(ptr, len)` は有効な読み取り可能領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_bundle_open(ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    let leaked: &'static mut [u8] = Box::leak(data.into_boxed_slice());
    let (leak_ptr, leak_len) = (leaked.as_mut_ptr(), leaked.len());
    match Bundle::parse(leaked) {
        Ok(b) => slot_insert(
            &BUNDLES,
            OwnedBundle {
                ptr: leak_ptr,
                len: leak_len,
                bundle: ManuallyDrop::new(b),
            },
        ),
        Err(e) => {
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(leak_ptr, leak_len)) });
            LAST_ERROR.with(|er| *er.borrow_mut() = e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tig_bundle_close(handle: i32) {
    BUNDLES.with(|slots| {
        if let Some(slot) = slots.borrow_mut().get_mut(handle as usize) {
            *slot = None;
        }
    });
}

fn with_bundle<R>(handle: i32, f: impl FnOnce(&Bundle<'static>) -> R) -> Option<R> {
    BUNDLES.with(|slots| {
        slots
            .borrow()
            .get(handle as usize)
            .and_then(|s| s.as_ref())
            .map(|owned| f(&owned.bundle))
    })
}

/// refs の一覧を JSON で返す。
#[unsafe(no_mangle)]
pub extern "C" fn tig_refs_json(handle: i32) -> *mut u8 {
    let Some(json) = with_bundle(handle, |bundle| {
        let mut json = String::from("[");
        for (i, (name, oid)) in bundle.refs.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str("{\"name\":\"");
            json_escape_into(&mut json, name);
            json.push_str(&format!("\",\"oid\":\"{oid}\"}}"));
        }
        json.push(']');
        json
    }) else {
        return fail("invalid bundle handle");
    };
    out_bytes(json.as_bytes())
}

/// 履歴を committer date 順の JSON で返す。`(ref_ptr, ref_len)` が空なら全 ref。
///
/// # Safety
/// `(ref_ptr, ref_len)` は `ref_len == 0` でない限り有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_log_json(
    handle: i32,
    ref_ptr: *const u8,
    ref_len: usize,
    limit: u32,
) -> *mut u8 {
    let ref_name = if ref_len == 0 {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(ref_ptr, ref_len) })
    };

    let Some(result) = with_bundle(handle, |bundle| -> Result<String, String> {
        let mut walk = Walk::new(&bundle.pack);
        match ref_name {
            Some(name) => {
                let oid = bundle
                    .find_ref(name)
                    .ok_or_else(|| format!("ref not found: {}", String::from_utf8_lossy(name)))?;
                walk.push(oid).map_err(|e| e.to_string())?;
            }
            None => {
                for (_, oid) in &bundle.refs {
                    walk.push(*oid).map_err(|e| e.to_string())?;
                }
            }
        }

        let mut json = String::from("[");
        for (i, item) in walk.take(limit as usize).enumerate() {
            let walked = item.map_err(|e| e.to_string())?;
            let commit = walked.commit().map_err(|e| e.to_string())?;
            let subject = commit.message.split(|&b| b == b'\n').next().unwrap_or(b"");
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"oid\":\"{}\",\"time\":{},\"tz\":\"",
                walked.oid, commit.committer.time
            ));
            json_escape_into(&mut json, commit.committer.tz);
            json.push_str("\",\"author\":\"");
            json_escape_into(&mut json, commit.author.name);
            json.push_str("\",\"subject\":\"");
            json_escape_into(&mut json, subject);
            json.push_str("\",\"parents\":[");
            for (j, parent) in commit.parents.iter().enumerate() {
                if j > 0 {
                    json.push(',');
                }
                json.push_str(&format!("\"{parent}\""));
            }
            json.push_str("]}");
        }
        json.push(']');
        Ok(json)
    }) else {
        return fail("invalid bundle handle");
    };
    match result {
        Ok(json) => out_bytes(json.as_bytes()),
        Err(e) => fail(e),
    }
}

/// object の body を返す。
///
/// # Safety
/// `(hex_ptr, hex_len)` は有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_cat(handle: i32, hex_ptr: *const u8, hex_len: usize) -> *mut u8 {
    let hex = unsafe { std::slice::from_raw_parts(hex_ptr, hex_len) };
    let Some(result) = with_bundle(handle, |bundle| -> Result<Vec<u8>, String> {
        let oid = Oid::from_hex(hex).map_err(|e| e.to_string())?;
        bundle
            .pack
            .read_object(&oid)
            .map_err(|e| e.to_string())?
            .map(|(_, body)| body)
            .ok_or_else(|| "object not found".to_owned())
    }) else {
        return fail("invalid bundle handle");
    };
    match result {
        Ok(body) => out_bytes(&body),
        Err(e) => fail(e),
    }
}

// --- clone ------------------------------------------------------------------

struct CloneSlot {
    driver: Option<CloneDriver>,
    /// 直近の `tig_clone_next_json` が生成した POST body。
    body: Vec<u8>,
}

/// clone driver を作る。`depth` 0 は全履歴、`(ref_ptr, ref_len)` が空なら全 ref。
///
/// # Safety
/// `(ref_ptr, ref_len)` は `ref_len == 0` でない限り有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_clone_new(depth: u32, ref_ptr: *const u8, ref_len: usize) -> i32 {
    let want_ref = if ref_len == 0 {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(ref_ptr, ref_len) }.to_vec())
    };
    let opts = CloneOptions {
        depth: if depth == 0 { None } else { Some(depth) },
        want_ref,
    };
    slot_insert(
        &CLONES,
        CloneSlot {
            driver: Some(CloneDriver::new(opts)),
            body: Vec::new(),
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn tig_clone_close(handle: i32) {
    CLONES.with(|slots| {
        if let Some(slot) = slots.borrow_mut().get_mut(handle as usize) {
            *slot = None;
        }
    });
}

fn with_clone<R>(handle: i32, f: impl FnOnce(&mut CloneSlot) -> R) -> Option<R> {
    CLONES.with(|slots| {
        slots
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|s| s.as_mut())
            .map(f)
    })
}

/// 次に送るべき request を JSON で返す: {"done":bool, "method":..., "path":...}。
/// POST body は `tig_clone_body` で取り出す。
#[unsafe(no_mangle)]
pub extern "C" fn tig_clone_next_json(handle: i32) -> *mut u8 {
    let Some(json) = with_clone(handle, |slot| {
        let next = slot.driver.as_ref().and_then(|d| match d.next_request() {
            None => None,
            Some(Request::Get { path }) => Some(("GET", path, Vec::new())),
            Some(Request::Post { path, body }) => Some(("POST", path, body)),
        });
        request_json(&mut slot.body, next)
    }) else {
        return fail("invalid clone handle");
    };
    out_bytes(json.as_bytes())
}

/// request を JSON にし、POST body を `slot_body` に控える (clone / push 共通)。
fn request_json(slot_body: &mut Vec<u8>, next: Option<(&str, &str, Vec<u8>)>) -> String {
    match next {
        None => {
            slot_body.clear();
            "{\"done\":true}".to_owned()
        }
        Some((method, path, body)) => {
            *slot_body = body;
            format!("{{\"done\":false,\"method\":\"{method}\",\"path\":\"{path}\"}}")
        }
    }
}

/// 直近の request の POST body (GET のときは空)。
#[unsafe(no_mangle)]
pub extern "C" fn tig_clone_body(handle: i32) -> *mut u8 {
    let Some(body) = with_clone(handle, |slot| slot.body.clone()) else {
        return fail("invalid clone handle");
    };
    out_bytes(&body)
}

/// response body を渡して状態を進める。0 = 成功、-1 = 失敗。
///
/// # Safety
/// `(ptr, len)` は有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_clone_response(handle: i32, ptr: *const u8, len: usize) -> i32 {
    let body = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Some(result) = with_clone(handle, |slot| {
        let Some(driver) = &mut slot.driver else {
            return Err("clone already finished".to_owned());
        };
        driver.on_response(body).map_err(|e| e.to_string())
    }) else {
        return fail::<i32>("invalid clone handle") - 1;
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail::<i32>(e) - 1,
    }
}

/// 完了した clone から bundle のバイト列を構成して返す。driver は消費される。
#[unsafe(no_mangle)]
pub extern "C" fn tig_clone_finish_bundle(handle: i32) -> *mut u8 {
    let Some(result) = with_clone(handle, |slot| -> Result<Vec<u8>, String> {
        let driver = slot
            .driver
            .take()
            .ok_or_else(|| "clone already finished".to_owned())?;
        let outcome = driver.finish().map_err(|e| e.to_string())?;
        let refs: Vec<(&[u8], Oid)> = outcome
            .refs
            .iter()
            .map(|e| (e.name.as_slice(), e.oid))
            .collect();
        bundle::write(&refs, &outcome.shallow, &outcome.pack).map_err(|e| e.to_string())
    }) else {
        return fail("invalid clone handle");
    };
    match result {
        Ok(data) => out_bytes(&data),
        Err(e) => fail(e),
    }
}

// --- push -------------------------------------------------------------------

struct PushSlot {
    driver: Option<PushDriver>,
    /// 直近の `tig_push_next_json` が生成した POST body。
    body: Vec<u8>,
}

/// 開いている bundle から push driver を作り handle (>= 0) を返す。失敗時は -1。
///
/// `(ref_ptr, ref_len)` が空なら bundle 内の refs/* を全て同名で push する。
/// 指定があればその ref を `(to_ptr, to_len)` の名前 (空なら同名) へ push する。
/// pack は bundle の全 object を詰め直す (remote が既に持つ object が混ざっても
/// 害はない)。
///
/// # Safety
/// `(ref_ptr, ref_len)` と `(to_ptr, to_len)` は長さ 0 でない限り有効な領域で
/// あること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_push_new(
    bundle_handle: i32,
    ref_ptr: *const u8,
    ref_len: usize,
    to_ptr: *const u8,
    to_len: usize,
) -> i32 {
    let src_ref = if ref_len == 0 {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(ref_ptr, ref_len) }.to_vec())
    };
    let dst_ref = if to_len == 0 {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(to_ptr, to_len) }.to_vec())
    };
    if src_ref.is_none() && dst_ref.is_some() {
        return fail::<i32>("destination ref requires a source ref") - 1;
    }
    let Some(result) = with_bundle(bundle_handle, |bundle| -> Result<PushDriver, String> {
        let updates: Vec<(Vec<u8>, Oid)> = match &src_ref {
            Some(name) => {
                let oid = bundle
                    .find_ref(name)
                    .ok_or_else(|| format!("ref not found: {}", String::from_utf8_lossy(name)))?;
                vec![(dst_ref.clone().unwrap_or_else(|| name.clone()), oid)]
            }
            None => bundle
                .refs
                .iter()
                .filter(|(name, _)| name.starts_with(b"refs/"))
                .map(|(name, oid)| (name.to_vec(), *oid))
                .collect(),
        };
        if updates.is_empty() {
            return Err("no refs to push".to_owned());
        }
        let objects: Vec<(Kind, Vec<u8>)> = bundle
            .pack
            .oids()
            .map(|oid| {
                bundle
                    .pack
                    .read_object(oid)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "object disappeared".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let borrowed: Vec<(Kind, &[u8])> =
            objects.iter().map(|(k, b)| (*k, b.as_slice())).collect();
        Ok(PushDriver::new(updates, pack::write_pack(&borrowed)))
    }) else {
        return fail::<i32>("invalid bundle handle") - 1;
    };
    match result {
        Ok(driver) => slot_insert(
            &PUSHES,
            PushSlot {
                driver: Some(driver),
                body: Vec::new(),
            },
        ),
        Err(e) => fail::<i32>(e) - 1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tig_push_close(handle: i32) {
    PUSHES.with(|slots| {
        if let Some(slot) = slots.borrow_mut().get_mut(handle as usize) {
            *slot = None;
        }
    });
}

fn with_push<R>(handle: i32, f: impl FnOnce(&mut PushSlot) -> R) -> Option<R> {
    PUSHES.with(|slots| {
        slots
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|s| s.as_mut())
            .map(f)
    })
}

/// 次に送るべき request を JSON で返す (形式は `tig_clone_next_json` と同じ)。
/// POST は Content-Type `application/x-git-receive-pack-request` で送ること。
#[unsafe(no_mangle)]
pub extern "C" fn tig_push_next_json(handle: i32) -> *mut u8 {
    let Some(json) = with_push(handle, |slot| {
        let next = slot.driver.as_ref().and_then(|d| match d.next_request() {
            None => None,
            Some(push::Request::Get { path }) => Some(("GET", path, Vec::new())),
            Some(push::Request::Post { path, body }) => Some(("POST", path, body)),
        });
        request_json(&mut slot.body, next)
    }) else {
        return fail("invalid push handle");
    };
    out_bytes(json.as_bytes())
}

/// 直近の request の POST body (GET のときは空)。
#[unsafe(no_mangle)]
pub extern "C" fn tig_push_body(handle: i32) -> *mut u8 {
    let Some(body) = with_push(handle, |slot| slot.body.clone()) else {
        return fail("invalid push handle");
    };
    out_bytes(&body)
}

/// response body を渡して状態を進める。0 = 成功、-1 = 失敗。
///
/// # Safety
/// `(ptr, len)` は有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_push_response(handle: i32, ptr: *const u8, len: usize) -> i32 {
    let body = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Some(result) = with_push(handle, |slot| {
        let Some(driver) = &mut slot.driver else {
            return Err("push already finished".to_owned());
        };
        driver.on_response(body).map_err(|e| e.to_string())
    }) else {
        return fail::<i32>("invalid push handle") - 1;
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail::<i32>(e) - 1,
    }
}

/// 完了した push の結果を JSON で返す。driver は消費される。
///
/// `{"up_to_date":true}` または
/// `{"up_to_date":false,"success":bool,"results":[{"ref":...,"error":null|...}]}`。
#[unsafe(no_mangle)]
pub extern "C" fn tig_push_finish_json(handle: i32) -> *mut u8 {
    let Some(result) = with_push(handle, |slot| -> Result<String, String> {
        let driver = slot
            .driver
            .take()
            .ok_or_else(|| "push already finished".to_owned())?;
        let outcome = driver.finish().map_err(|e| e.to_string())?;
        let Some(report) = outcome.report else {
            return Ok("{\"up_to_date\":true}".to_owned());
        };
        let mut json = format!(
            "{{\"up_to_date\":false,\"success\":{},\"results\":[",
            report.is_success()
        );
        for (i, (name, error)) in report.results.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str("{\"ref\":\"");
            json_escape_into(&mut json, name);
            json.push_str("\",\"error\":");
            match error {
                None => json.push_str("null"),
                Some(msg) => {
                    json.push('"');
                    json_escape_into(&mut json, msg);
                    json.push('"');
                }
            }
            json.push('}');
        }
        json.push_str("]}");
        Ok(json)
    }) else {
        return fail("invalid push handle");
    };
    match result {
        Ok(json) => out_bytes(json.as_bytes()),
        Err(e) => fail(e),
    }
}

// --- JSON -------------------------------------------------------------------

fn json_escape_into(out: &mut String, bytes: &[u8]) {
    for c in String::from_utf8_lossy(bytes).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape() {
        let mut s = String::new();
        json_escape_into(&mut s, b"a\"b\\c\nd\x01");
        assert_eq!(s, "a\\\"b\\\\c\\nd\\u0001");
    }

    /// bundle を開いて push driver を C ABI 経由で一巡させる。HTTP は
    /// 固定の advertisement / report-status で代替する (core の push テストと
    /// 同じ形式)。
    #[test]
    fn push_handle_round_trip() {
        use tig_core::object::{Sig, compute_oid};
        use tig_core::{build, bundle, pkt};

        let tree = build::tree(&[]).unwrap();
        let sig = Sig {
            name: b"T",
            email: b"t@example.com",
            time: 0,
            tz: b"+0000",
        };
        let commit =
            build::commit(compute_oid(Kind::Tree, &tree), &[], &sig, &sig, b"m\n").unwrap();
        let commit_oid = compute_oid(Kind::Commit, &commit);
        let pack = pack::write_pack(&[(Kind::Commit, &commit), (Kind::Tree, &tree)]);
        let data = bundle::write(&[(b"refs/heads/main", commit_oid)], &[], &pack).unwrap();

        let bundle_handle = unsafe { tig_bundle_open(data.as_ptr(), data.len()) };
        assert!(bundle_handle >= 0);
        let to = b"refs/heads/renamed";
        let handle =
            unsafe { tig_push_new(bundle_handle, std::ptr::null(), 0, to.as_ptr(), to.len()) };
        assert_eq!(handle, -1, "destination without source ref must fail");
        assert_eq!(
            read_out(tig_last_error()),
            "destination ref requires a source ref"
        );
        let src = b"refs/heads/main";
        let handle = unsafe {
            tig_push_new(
                bundle_handle,
                src.as_ptr(),
                src.len(),
                to.as_ptr(),
                to.len(),
            )
        };
        assert!(handle >= 0, "{}", read_out(tig_last_error()));

        assert_eq!(
            read_out(tig_push_next_json(handle)),
            "{\"done\":false,\"method\":\"GET\",\"path\":\"info/refs?service=git-receive-pack\"}"
        );
        let mut adv = Vec::new();
        pkt::write_line(&mut adv, b"# service=git-receive-pack");
        pkt::write_flush(&mut adv);
        pkt::write_line(
            &mut adv,
            b"0000000000000000000000000000000000000000 capabilities^{}\0report-status",
        );
        pkt::write_flush(&mut adv);
        assert_eq!(
            unsafe { tig_push_response(handle, adv.as_ptr(), adv.len()) },
            0
        );

        let next = read_out(tig_push_next_json(handle));
        assert!(next.contains("\"POST\""), "{next}");
        let body = read_result_bytes(tig_push_body(handle));
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains(&format!(
                "0000000000000000000000000000000000000000 {commit_oid} refs/heads/renamed"
            )),
            "{text}"
        );
        assert!(text.contains("PACK"));

        let mut report = Vec::new();
        pkt::write_line(&mut report, b"unpack ok");
        pkt::write_line(&mut report, b"ok refs/heads/renamed");
        pkt::write_flush(&mut report);
        assert_eq!(
            unsafe { tig_push_response(handle, report.as_ptr(), report.len()) },
            0
        );
        assert_eq!(read_out(tig_push_next_json(handle)), "{\"done\":true}");
        assert_eq!(
            read_out(tig_push_finish_json(handle)),
            "{\"up_to_date\":false,\"success\":true,\"results\":[{\"ref\":\"refs/heads/renamed\",\"error\":null}]}"
        );
        tig_push_close(handle);
        tig_bundle_close(bundle_handle);
    }

    fn read_result_bytes(ptr: *mut u8) -> Vec<u8> {
        assert!(!ptr.is_null());
        let len = u32::from_le_bytes(
            unsafe { std::slice::from_raw_parts(ptr, 4) }
                .try_into()
                .unwrap(),
        ) as usize;
        let out = unsafe { std::slice::from_raw_parts(ptr.add(4), len) }.to_vec();
        unsafe { tig_free(ptr) };
        out
    }

    fn read_out(ptr: *mut u8) -> String {
        String::from_utf8(read_result_bytes(ptr)).unwrap()
    }

    #[test]
    fn json_escape_invalid_utf8() {
        let mut s = String::new();
        json_escape_into(&mut s, &[0xff, b'x']);
        assert_eq!(s, "\u{fffd}x");
    }
}
