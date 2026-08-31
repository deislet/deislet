//! `ctx.services.blob`: object storage, backed by deis-store's blob service.
//!
//! Errors are returned as [`JsErrorBox`] rather than `std::io::Error`, because
//! only the former's message survives the trip into JavaScript; see
//! `super::ops::service_unavailable`.
//!
//! Two shapes here are worth knowing before reading the ops. Bodies cross as
//! buffers in both directions - `#[buffer] JsBuffer` in, [`ToJsBuffer`] out -
//! because `#[serde] Vec<u8>` turns a body into a JavaScript array of numbers
//! on the way out and refuses every typed array on the way in. And a
//! conditional write that lost its race is `null` rather than an exception: it
//! is the one failure an application answers by reading the object again and
//! trying, and every exception these ops raise is one it must not retry as it
//! stands.

use super::blob_client::{
    BlobClient, BlobListRequest, BlobMeta, BlobPrecondition, Conditional, RangeRequest,
};
use super::ops::{service_call_failed, service_unavailable};
use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

fn client(state: &Rc<RefCell<OpState>>) -> Result<BlobClient, JsErrorBox> {
    let state_borrow = state.borrow();
    state_borrow
        .try_borrow::<BlobClient>()
        .cloned()
        .ok_or_else(|| service_unavailable("blob"))
}

/// What the store knows about one object, as JavaScript receives it.
///
/// `etag` is the object's version identity: hand it back in an `onlyIf` to
/// make the next write conditional, or put it in an HTTP `ETag` header. It is
/// the sha256 of the body, which is why it can also be verified.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobMetaResult {
    pub key: String,
    pub etag: String,
    pub size: u64,
    pub content_type: Option<String>,
    /// Unix seconds at which this key was last written.
    pub uploaded_at: i64,
}

impl From<BlobMeta> for BlobMetaResult {
    fn from(meta: BlobMeta) -> Self {
        Self {
            key: meta.key,
            etag: meta.etag,
            size: meta.size,
            content_type: meta.content_type,
            uploaded_at: meta.created_at,
        }
    }
}

/// One object read, with the bytes as a `Uint8Array`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobReadResult {
    #[serde(flatten)]
    pub meta: BlobMetaResult,
    pub body: ToJsBuffer,
    /// Which bytes `body` holds; absent for a whole-object read. With `size`
    /// these are the three numbers a `Content-Range` header carries, resolved
    /// by the store so that no two layers can disagree about them.
    pub range: Option<BlobRangeResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobRangeResult {
    pub offset: u64,
    pub length: u64,
}

/// Which bytes of an object `ctx.services.blob.get` asked for.
///
/// `header` is a raw HTTP `Range` header value, which is how an application
/// forwards the one it was given; the rest is the same range spelled as
/// numbers. `js/runtime.js` sends one shape or the other, never both, and the
/// parsing of the header form lives here rather than there so that exactly one
/// implementation of RFC 9110's grammar exists in this platform.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobRangeOption {
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default)]
    pub suffix: Option<i64>,
}

/// What `ctx.services.blob.get` may ask for beyond the key.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobGetOptions {
    #[serde(default)]
    pub range: Option<BlobRangeOption>,
}

/// What `ctx.services.blob.put` and `.delete` may require of the object first.
///
/// Exactly one of the two is set; both being set is a caller bug and is
/// refused rather than silently resolved in some order the caller cannot see.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobConditionOption {
    #[serde(default)]
    pub if_match: Option<String>,
    #[serde(default)]
    pub if_none_match: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobPutOptions {
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub only_if: Option<BlobConditionOption>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobDeleteOptions {
    #[serde(default)]
    pub only_if: Option<BlobConditionOption>,
}

/// What `ctx.services.blob.list` may ask a listing for.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobListOptions {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub delimiter: Option<String>,
    #[serde(default)]
    pub limit: Option<i32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// One page of a listing.
///
/// `cursor` is what to resume from and `null` means the scan is finished. It
/// is deliberately the only signal: a page can come back full and still be the
/// last one, so "was this page short?" is not an answer to "is there more?".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobListPageResult {
    pub objects: Vec<BlobMetaResult>,
    pub prefixes: Vec<String>,
    pub cursor: Option<String>,
}

/// Turn what JavaScript asked for into the range the client sends, refusing
/// anything that names no bytes before a call is made.
fn range_of(option: BlobRangeOption) -> Result<RangeRequest, JsErrorBox> {
    if let Some(header) = option.header {
        return parse_range_header(&header);
    }
    let whole = |name: &str, value: i64| -> Result<u64, JsErrorBox> {
        u64::try_from(value).map_err(|_| {
            JsErrorBox::type_error(format!(
                "blob.get: range.{name} must not be negative, got {value}"
            ))
        })
    };
    match (option.offset, option.length, option.suffix) {
        (None, None, Some(suffix)) => Ok(RangeRequest::Suffix(whole("suffix", suffix)?)),
        (_, _, Some(_)) => Err(JsErrorBox::type_error(
            "blob.get: range.suffix asks for the last bytes of the object, so it cannot be \
             combined with range.offset or range.length"
                .to_string(),
        )),
        (offset, length, None) => Ok(RangeRequest::Span {
            offset: match offset {
                Some(offset) => whole("offset", offset)?,
                None => 0,
            },
            length: match length {
                Some(length) => Some(whole("length", length)?),
                None => None,
            },
        }),
    }
}

/// Parse an HTTP `Range` header value the way RFC 9110 defines it, for the one
/// form an object store can answer: a single byte range.
///
/// A multi-range header (`bytes=0-9,20-29`) is refused rather than partly
/// honoured. Answering only its first range would give the application a body
/// that does not match the header it forwarded, and there is no way to build a
/// correct `206` out of that.
fn parse_range_header(header: &str) -> Result<RangeRequest, JsErrorBox> {
    let refuse = |why: &str| {
        JsErrorBox::type_error(format!(
            "blob.get: {header:?} is not a byte range this store can answer: {why}. Use \
             \"bytes=<first>-<last>\", \"bytes=<first>-\" or \"bytes=-<count>\""
        ))
    };
    let spec = header
        .trim()
        .strip_prefix("bytes=")
        .ok_or_else(|| refuse("it does not start with `bytes=`"))?
        .trim();
    if spec.contains(',') {
        return Err(refuse("it asks for more than one range"));
    }
    let (first, last) = spec
        .split_once('-')
        .ok_or_else(|| refuse("it has no `-` between the two ends"))?;
    let number = |text: &str| -> Result<u64, JsErrorBox> {
        text.trim()
            .parse::<u64>()
            .map_err(|_| refuse("one of its ends is not a whole number of bytes"))
    };
    match (first.trim().is_empty(), last.trim().is_empty()) {
        // `bytes=-<n>`: the last n bytes.
        (true, false) => Ok(RangeRequest::Suffix(number(last)?)),
        // `bytes=<first>-`: from first to the end.
        (false, true) => Ok(RangeRequest::Span {
            offset: number(first)?,
            length: None,
        }),
        (false, false) => {
            let (first, last) = (number(first)?, number(last)?);
            if last < first {
                return Err(refuse("its last byte comes before its first"));
            }
            Ok(RangeRequest::Span {
                offset: first,
                // Inclusive on the wire, a count here: `bytes=0-0` is one
                // byte, and an off-by-one at this line would hand every
                // application one byte too few.
                length: Some(last - first + 1),
            })
        }
        (true, true) => Err(refuse("it names neither end")),
    }
}

/// Turn an `onlyIf` into the precondition the client sends.
fn precondition_of(
    call: &str,
    option: BlobConditionOption,
) -> Result<Option<BlobPrecondition>, JsErrorBox> {
    match (option.if_match, option.if_none_match) {
        (Some(_), Some(_)) => Err(JsErrorBox::type_error(format!(
            "{call}: onlyIf takes ifMatch or ifNoneMatch, not both; two conditions on one write \
             have no defined order and the write was not attempted"
        ))),
        (Some(etag), None) => Ok(Some(BlobPrecondition::IfMatch(etag))),
        (None, Some(etag)) => Ok(Some(BlobPrecondition::IfNoneMatch(etag))),
        // An empty `onlyIf` object is an unconditional write rather than an
        // error: it is what a caller building one from optional inputs ends up
        // with, and refusing it would make every such caller special-case it.
        (None, None) => Ok(None),
    }
}

/// Read an object, whole or one slice of it.
///
/// Answers `null` when the object is not there. An unsatisfiable range is an
/// exception instead, because "this object has no such bytes" and "there is no
/// such object" are different answers and an application forwarding them into
/// HTTP owes its caller 416 for one and 404 for the other.
#[op2]
#[serde]
async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[serde] options: BlobGetOptions,
) -> Result<Option<BlobReadResult>, JsErrorBox> {
    let range = match options.range {
        Some(range) => Some(range_of(range)?),
        None => None,
    };
    let found = client(&state)?
        .read(key, range)
        .await
        .map_err(|e| service_call_failed("blob.get", e))?;
    Ok(found.map(|body| BlobReadResult {
        meta: BlobMetaResult::from(body.meta),
        body: ToJsBuffer::from(body.data),
        range: body.range.map(|r| BlobRangeResult {
            offset: r.offset,
            length: r.length,
        }),
    }))
}

/// What the store knows about one object, without moving its body.
#[op2]
#[serde]
async fn op_blob_head(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<BlobMetaResult>, JsErrorBox> {
    let found = client(&state)?
        .head(key)
        .await
        .map_err(|e| service_call_failed("blob.head", e))?;
    Ok(found.map(BlobMetaResult::from))
}

/// One page of this application's objects.
#[op2]
#[serde]
async fn op_blob_list(
    state: Rc<RefCell<OpState>>,
    #[serde] options: BlobListOptions,
) -> Result<BlobListPageResult, JsErrorBox> {
    let page = client(&state)?
        .list(BlobListRequest {
            prefix: options.prefix.unwrap_or_default(),
            delimiter: options.delimiter,
            limit: options.limit,
            cursor: options.cursor,
        })
        .await
        .map_err(|e| service_call_failed("blob.list", e))?;
    Ok(BlobListPageResult {
        objects: page.objects.into_iter().map(BlobMetaResult::from).collect(),
        prefixes: page.prefixes,
        cursor: page.cursor,
    })
}

/// Store an object.
///
/// Takes a `Uint8Array` (or any `ArrayBufferView`) straight from V8: `#[serde]
/// Vec<u8>` here refused every typed array with "invalid type", so the only
/// way through was an array of plain numbers, which is not what any caller
/// building bytes would pass.
///
/// Answers `null` when an `onlyIf` was given and did not hold. That is the
/// only way this returns nothing, so an application can branch on it without
/// parsing a message.
#[op2]
#[serde]
async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[buffer] data: JsBuffer,
    #[serde] options: BlobPutOptions,
) -> Result<Option<BlobMetaResult>, JsErrorBox> {
    let precondition = match options.only_if {
        Some(only_if) => precondition_of("blob.put", only_if)?,
        None => None,
    };
    let written = client(&state)?
        .write(key, data.to_vec(), options.content_type, precondition)
        .await
        .map_err(|e| service_call_failed("blob.put", e))?;
    Ok(match written {
        Conditional::Done(meta) => Some(BlobMetaResult::from(meta)),
        Conditional::Refused { .. } => None,
    })
}

/// Remove an object. Deleting a key that is already absent succeeds.
///
/// Answers `false` when an `onlyIf` was given and did not hold, and `true`
/// otherwise - including for a key that was not there, which is what makes an
/// unconditional delete idempotent.
#[op2]
async fn op_blob_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[serde] options: BlobDeleteOptions,
) -> Result<bool, JsErrorBox> {
    let precondition = match options.only_if {
        Some(only_if) => precondition_of("blob.delete", only_if)?,
        None => None,
    };
    let removed = client(&state)?
        .delete(key, precondition)
        .await
        .map_err(|e| service_call_failed("blob.delete", e))?;
    Ok(matches!(removed, Conditional::Done(())))
}

extension!(
    deis_runtime_blob_ops,
    ops = [
        op_blob_get,
        op_blob_head,
        op_blob_list,
        op_blob_put,
        op_blob_delete
    ],
);

pub fn init_blob_extension() -> deno_core::Extension {
    deis_runtime_blob_ops::init()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(header: &str) -> RangeRequest {
        parse_range_header(header).expect("a range this store can answer")
    }

    #[test]
    fn a_closed_range_is_a_span_of_the_inclusive_count() {
        // `bytes=0-0` is one byte, not zero: the header's last index is
        // inclusive and the wire's length is a count.
        assert_eq!(
            parse("bytes=0-0"),
            RangeRequest::Span {
                offset: 0,
                length: Some(1)
            }
        );
        assert_eq!(
            parse("bytes=10-19"),
            RangeRequest::Span {
                offset: 10,
                length: Some(10)
            }
        );
    }

    #[test]
    fn an_open_ended_range_runs_to_the_end() {
        assert_eq!(
            parse("bytes=100-"),
            RangeRequest::Span {
                offset: 100,
                length: None
            }
        );
    }

    #[test]
    fn a_suffix_range_asks_for_the_last_bytes() {
        assert_eq!(parse("bytes=-500"), RangeRequest::Suffix(500));
    }

    #[test]
    fn surrounding_space_is_not_part_of_the_range() {
        assert_eq!(parse(" bytes= 5 - 9 "), parse("bytes=5-9"));
    }

    #[test]
    fn a_header_this_store_cannot_answer_is_refused_before_any_call() {
        for (header, why) in [
            ("items=0-9", "not a byte range"),
            ("bytes=0-9,20-29", "more than one range"),
            ("bytes=9-0", "last byte before the first"),
            ("bytes=-", "names neither end"),
            ("bytes=abc-def", "not a number"),
            ("0-9", "no unit"),
        ] {
            assert!(
                parse_range_header(header).is_err(),
                "{header} should be refused: {why}"
            );
        }
    }

    #[test]
    fn a_numeric_range_needs_no_header_parsing() {
        assert_eq!(
            range_of(BlobRangeOption {
                offset: Some(4),
                length: Some(8),
                ..Default::default()
            })
            .unwrap(),
            RangeRequest::Span {
                offset: 4,
                length: Some(8)
            }
        );
        assert_eq!(
            range_of(BlobRangeOption {
                suffix: Some(16),
                ..Default::default()
            })
            .unwrap(),
            RangeRequest::Suffix(16)
        );
        // An empty option object reads the object from its first byte, which
        // is what `{}` means and is not an error.
        assert_eq!(
            range_of(BlobRangeOption::default()).unwrap(),
            RangeRequest::Span {
                offset: 0,
                length: None
            }
        );
    }

    #[test]
    fn a_suffix_cannot_be_combined_with_an_offset() {
        // The two mean different things about where the bytes start, and
        // silently preferring one would hand the application the wrong slice.
        let error = range_of(BlobRangeOption {
            offset: Some(0),
            suffix: Some(10),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot be combined"), "{error}");
    }

    #[test]
    fn a_negative_bound_is_refused_rather_than_wrapped() {
        // These arrive as JavaScript numbers, so a negative one is reachable
        // from an application; converting it would make `-1` mean the whole
        // address space.
        let error = range_of(BlobRangeOption {
            offset: Some(-1),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("must not be negative"), "{error}");
    }

    #[test]
    fn one_condition_at_a_time() {
        assert_eq!(
            precondition_of(
                "blob.put",
                BlobConditionOption {
                    if_match: Some("abc".to_string()),
                    if_none_match: None,
                }
            )
            .unwrap(),
            Some(BlobPrecondition::IfMatch("abc".to_string()))
        );
        assert_eq!(
            precondition_of(
                "blob.put",
                BlobConditionOption {
                    if_match: None,
                    if_none_match: Some("*".to_string()),
                }
            )
            .unwrap(),
            Some(BlobPrecondition::IfNoneMatch("*".to_string()))
        );
        assert_eq!(
            precondition_of("blob.put", BlobConditionOption::default()).unwrap(),
            None
        );
        let error = precondition_of(
            "blob.put",
            BlobConditionOption {
                if_match: Some("a".to_string()),
                if_none_match: Some("b".to_string()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not both"), "{error}");
    }
}
