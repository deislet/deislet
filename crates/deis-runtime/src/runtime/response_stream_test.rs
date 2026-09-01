#[cfg(test)]
mod tests {
    use super::super::{ResponseBody, ResponseStreams};
    use crate::runtime::response_stream::StreamOwner;
    use bytes::Bytes;
    use std::future::poll_fn;
    use std::time::{Duration, Instant};

    /// Everything the reading end has, drained until the producer's end is
    /// gone. Written here rather than in each test because the two facts a
    /// stream ends with -- the bytes and why it stopped -- can only be read in
    /// that order.
    async fn drain(body: ResponseBody) -> (Vec<u8>, Option<String>) {
        let ResponseBody::Stream(mut reader) = body else {
            panic!("this test opened a stream and expects one back");
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = poll_fn(|cx| reader.poll_chunk(cx)).await {
            bytes.extend_from_slice(&chunk);
        }
        (bytes, reader.failure())
    }

    fn in_a_minute() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A stand-in for the tenant and invocation a real stream is opened
    /// under. The tests below are about the queue and the deadlines, not
    /// about who the body belongs to, but a stream always has an owner --
    /// that is what makes the end of one findable.
    fn owner() -> StreamOwner {
        StreamOwner {
            tenant: "demo.production".to_string(),
            trace_id: "trace-under-test".to_string(),
            target: "http://localhost/stream".to_string(),
        }
    }

    /// The outbound-call allowance a stream inherits from the invocation that
    /// opened it. Nothing in this file makes an outbound call, so any
    /// allowance will do; what matters elsewhere is that the registry holds
    /// one and drops it with the stream.
    fn allowance() -> std::rc::Rc<crate::runtime::ops::SubrequestAllowance> {
        std::rc::Rc::new(crate::runtime::ops::SubrequestAllowance::new(10))
    }

    #[tokio::test]
    async fn the_queue_holds_two_chunks_and_then_makes_the_producer_wait() {
        // This is where the memory bound is. A queue that accepted a third
        // chunk would not have moved the problem, it would have hidden it: a
        // client that never reads would let a producer put the whole body in
        // the node's memory, which is the thing streaming exists to avoid.
        let mut streams = ResponseStreams::default();
        let (id, mut reader) = streams.open(in_a_minute(), owner(), allowance());
        let sink = streams.sinks().pop().unwrap();

        sink.send(Bytes::from_static(b"one")).await.unwrap();
        sink.send(Bytes::from_static(b"two")).await.unwrap();

        let third = sink.try_send(Bytes::from_static(b"three"));
        assert!(
            third.is_err(),
            "the third chunk was accepted with nothing read, so the queue is not bounded"
        );

        let first = poll_fn(|cx| reader.poll_chunk(cx)).await;
        assert_eq!(first.as_deref(), Some(&b"one"[..]));
        sink.try_send(Bytes::from_static(b"three"))
            .expect("a slot freed by the reader must let the producer go on");

        assert_eq!(streams.len(), 1);
        streams.close(id);
        assert!(streams.is_empty());
    }

    #[tokio::test]
    async fn a_producer_that_failed_is_told_apart_from_one_that_finished() {
        // The client was answered 200 before either ending existed, so the only
        // way it can tell a complete body from a truncated one is that the
        // truncated one's transfer fails. That distinction is this pair of
        // assertions.
        let mut finished = ResponseStreams::default();
        let (id, reader) = finished.open(in_a_minute(), owner(), allowance());
        finished.sinks()[0]
            .send(Bytes::from_static(b"all of it"))
            .await
            .unwrap();
        finished.close(id);
        let (bytes, failure) = drain(ResponseBody::Stream(reader)).await;
        assert_eq!(bytes, b"all of it");
        assert_eq!(failure, None, "a stream that finished must not look failed");

        let mut broken = ResponseStreams::default();
        let (id, reader) = broken.open(in_a_minute(), owner(), allowance());
        broken.sinks()[0]
            .send(Bytes::from_static(b"half of "))
            .await
            .unwrap();
        broken.close_with_failure(id, "the producer threw".to_string());
        let (bytes, failure) = drain(ResponseBody::Stream(reader)).await;
        assert_eq!(
            bytes, b"half of ",
            "everything produced before the failure still belongs to the client"
        );
        assert_eq!(
            failure.as_deref(),
            Some("the producer threw"),
            "the reader has to be able to see, once the bytes have run out, that they ran out \
             for a reason"
        );
    }

    #[test]
    fn a_stream_is_expired_by_its_own_deadline_and_abandoned_by_its_own_client() {
        // The node has to be able to name *which* stream to close: an isolate
        // serving server-sent events has one open per connected browser, and
        // closing all of them because one ran out would be a far worse bug than
        // the one being fixed.
        let mut streams = ResponseStreams::default();
        let (long, _long_reader) = streams.open(
            Instant::now() + Duration::from_secs(60),
            owner(),
            allowance(),
        );
        let (short, _short_reader) = streams.open(
            Instant::now() - Duration::from_millis(1),
            owner(),
            allowance(),
        );
        let (gone, gone_reader) = streams.open(
            Instant::now() + Duration::from_secs(60),
            owner(),
            allowance(),
        );

        assert_eq!(streams.expired(Instant::now()), vec![short]);
        assert!(
            streams.abandoned().is_empty(),
            "nothing has hung up yet, so nothing is abandoned"
        );

        drop(gone_reader);
        assert_eq!(streams.abandoned(), vec![gone]);

        // The runtime thread sleeps until the first of them runs out, so the
        // answer has to be the short one's rather than any other.
        let earliest = streams
            .earliest_deadline()
            .expect("three streams are open, so one of them is the first to run out");
        assert!(
            earliest <= Instant::now(),
            "the earliest deadline is not the one that has already passed"
        );

        streams.close(short);
        streams.close(gone);
        assert_eq!(streams.ids(), vec![long]);
    }

    #[test]
    fn a_body_that_is_not_a_stream_still_takes_the_simple_road() {
        // Nearly every response is one of these two, and neither may acquire a
        // queue, a deadline or a producer on the way out. "No body at all" and
        // "a body of zero length" stay different answers: a 204 and a HEAD
        // carry the first, and inventing the second for them would put a
        // Content-Length of 0 on a response that must not have one.
        assert_eq!(ResponseBody::None.into_bytes(), None);
        assert_eq!(
            ResponseBody::Bytes(Vec::new()).into_bytes(),
            Some(Vec::new())
        );
        assert_eq!(
            ResponseBody::Bytes(b"hello".to_vec())
                .into_bytes()
                .as_deref(),
            Some(&b"hello"[..])
        );
    }
}
