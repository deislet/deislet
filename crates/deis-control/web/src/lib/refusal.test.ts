import { strict as assert } from 'node:assert'
import { test } from 'node:test'

import {
  ApiError,
  MAX_ADVISED_WAIT_MS,
  MAX_POLL_BACKOFF_MS,
  describeRefusal,
  nextPollDelayMs,
  refusalFrom,
  type RefusalState,
} from './refusal.ts'

/**
 * The console's half of `contracts/v1/control-error.schema.json`.
 *
 * These run outside a browser, on Node's own test runner and its own type
 * stripping, because everything under test here is deliberately free of
 * `fetch`, `window` and React: what a refusal means to an operator is a
 * question about the envelope, not about the DOM.
 *
 * What they are guarding is the thing the console got wrong for a while.
 * Every read reported `err.message` and nothing else, so a scope refusal, a
 * conflict, a rate limit and a crashed control plane all arrived on screen
 * as the same red sentence, and the server's `retryAfterSeconds` was parsed
 * off the wire and dropped - the console went on polling every two seconds
 * through a 429 that had asked it for thirty. Both are asserted below: the
 * classes must stay distinguishable, and the advice must reach the poller.
 */

/** The envelope, as `crates/deis-control/src/api/error.rs` writes it. */
function envelope(error: Record<string, unknown>): string {
  return JSON.stringify({ error })
}

const REQUEST_ID = '0199a1f4-8a1c-7b3e-9f2a-2b6f0c1d4e5a'

test('a rate limit carries the server-advised wait through to the poller', () => {
  const refused = refusalFrom(
    429,
    'Too Many Requests',
    { requestId: REQUEST_ID },
    envelope({
      code: 'service-unavailable',
      message: 'Too many reads of the metrics summary.',
      requestId: REQUEST_ID,
      violations: [],
      retryable: true,
      retryAfterSeconds: 30,
    })
  )

  assert.equal(refused.status, 429)
  assert.equal(refused.retryable, true)
  assert.equal(refused.retryAfterSeconds, 30)

  const state = describeRefusal(refused, 'the metrics summary')
  assert.equal(state.kind, 'rate-limited')
  assert.equal(state.retryAfterSeconds, 30)
  // The read polls every two seconds; after this refusal it must wait the
  // thirty the control plane asked for, not the two it prefers.
  assert.equal(nextPollDelayMs(2000, 1, state), 30_000)
})

test('a wait is only taken from a refusal that says it is retryable', () => {
  // `retryAfterSeconds` is meaningless on a code that will answer the same
  // way forever, and honouring one would freeze the read for no reason.
  const refused = refusalFrom(
    403,
    'Forbidden',
    {},
    envelope({
      code: 'insufficient-scope',
      message: 'This key may not read platform diagnostics.',
      requestId: REQUEST_ID,
      violations: [],
      retryable: false,
      retryAfterSeconds: 600,
    })
  )

  assert.equal(refused.retryable, false)
  assert.equal(refused.retryAfterSeconds, undefined)
  assert.equal(describeRefusal(refused, 'the runtime fleet').retryAfterSeconds, undefined)
})

test('a Retry-After header is read when the body carries no advice', () => {
  const seconds = refusalFrom(503, 'Service Unavailable', { retryAfter: '12' }, '')
  assert.equal(seconds.retryAfterSeconds, 12)
  assert.equal(seconds.retryable, true)

  // The other spelling RFC 9110 allows, measured against a fixed clock.
  const now = Date.parse('2026-09-07T10:00:00Z')
  const dated = refusalFrom(
    503,
    'Service Unavailable',
    { retryAfter: 'Mon, 07 Sep 2026 10:00:45 GMT' },
    '',
    now
  )
  assert.equal(dated.retryAfterSeconds, 45)

  // A date already in the past is not a negative wait.
  const stale = refusalFrom(
    503,
    'Service Unavailable',
    { retryAfter: 'Mon, 07 Sep 2026 09:59:00 GMT' },
    '',
    now
  )
  assert.equal(stale.retryAfterSeconds, 0)

  // Anything else is no advice at all rather than a NaN wait.
  assert.equal(refusalFrom(503, 'Service Unavailable', { retryAfter: 'soon' }, '').retryAfterSeconds, undefined)
})

test('the refusal classes a console must tell apart are told apart', () => {
  const cases: Array<{ status: number; code: string; kind: string }> = [
    { status: 401, code: 'unauthenticated', kind: 'unauthorized' },
    { status: 403, code: 'insufficient-scope', kind: 'forbidden' },
    { status: 404, code: 'not-found', kind: 'missing' },
    { status: 409, code: 'revision-conflict', kind: 'conflict' },
    { status: 429, code: 'service-unavailable', kind: 'rate-limited' },
    { status: 503, code: 'service-unavailable', kind: 'unavailable' },
    { status: 500, code: 'internal', kind: 'server' },
    { status: 422, code: 'invalid-argument', kind: 'refused' },
  ]

  const states = cases.map(({ status, code }) =>
    describeRefusal(
      refusalFrom(
        status,
        '',
        {},
        envelope({
          code,
          message: `refused with ${code}`,
          requestId: REQUEST_ID,
          violations: [],
          retryable: status >= 500 || status === 429,
        })
      ),
      'the event log'
    )
  )

  states.forEach((state, index) => {
    assert.equal(state.kind, cases[index].kind, `status ${cases[index].status}`)
    assert.equal(state.code, cases[index].code)
    assert.equal(state.requestId, REQUEST_ID)
  })

  // The whole point: one red sentence for all of them is what this replaced,
  // so no two of these may render the same headline.
  const titles = new Set(states.map((state) => state.title))
  assert.equal(titles.size, states.length)
})

test('a database that is not answering is a wait, not a crash', () => {
  // A dependency the control plane dialled can come back as a 500; the
  // code is the more precise of the two and decides what the console says.
  const state = describeRefusal(
    refusalFrom(
      500,
      '',
      {},
      envelope({
        code: 'database-unavailable',
        message: 'The control plane could not reach its database.',
        requestId: REQUEST_ID,
        violations: [],
        retryable: true,
      })
    ),
    'the command history'
  )
  assert.equal(state.kind, 'unavailable')
})

test('the flat refusal and a bodiless one still reach the operator', () => {
  // The authorization gate answers outside the envelope, and a proxy in
  // front of the control plane answers with no JSON at all.
  const flat = refusalFrom(403, 'Forbidden', { requestId: REQUEST_ID }, '{"error":"missing api key"}')
  assert.equal(flat.sentence, 'missing api key')
  assert.equal(flat.code, undefined)
  assert.equal(flat.message, `missing api key (request ${REQUEST_ID})`)
  assert.equal(describeRefusal(flat, 'the control channels').kind, 'forbidden')

  const bodiless = refusalFrom(502, 'Bad Gateway', {}, '')
  assert.equal(bodiless.sentence, 'Bad Gateway')
  assert.equal(bodiless.retryable, true)
  assert.equal(describeRefusal(bodiless, 'the control channels').kind, 'server')
})

test('a read that never got an answer is reported, not swallowed', () => {
  const state = describeRefusal(new ApiError(0, 'Failed to fetch'), 'the runtime fleet')
  assert.equal(state.kind, 'offline')
  assert.equal(state.status, 0)
  assert.equal(state.message, 'Failed to fetch')

  // Something that never went through the request layer at all.
  const raw = describeRefusal(new TypeError('boom'), 'the runtime fleet')
  assert.equal(raw.kind, 'offline')
  assert.equal(raw.message, 'boom')
})

test('a refused poll backs off and a successful one does not', () => {
  const refusal: RefusalState = {
    kind: 'server',
    status: 500,
    code: 'internal',
    title: 'irrelevant here',
    message: 'irrelevant here',
    retryable: true,
    violations: [],
  }

  assert.equal(nextPollDelayMs(5000, 0, null), 5000)
  assert.equal(nextPollDelayMs(5000, 1, refusal), 10_000)
  assert.equal(nextPollDelayMs(5000, 2, refusal), 20_000)
  // A console is watched by a person: it slows down, it does not go away.
  assert.equal(nextPollDelayMs(5000, 40, refusal), MAX_POLL_BACKOFF_MS)

  // Server advice below the polling interval does not speed the poll up.
  assert.equal(nextPollDelayMs(5000, 1, { ...refusal, retryAfterSeconds: 1 }), 5000)
  // And advice long enough to make the tab useless is capped.
  assert.equal(nextPollDelayMs(5000, 1, { ...refusal, retryAfterSeconds: 86_400 }), MAX_ADVISED_WAIT_MS)
})
