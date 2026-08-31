// The span-updating half of the telemetry stub. See `telemetry.ts`: deno_fetch
// destructures these three off `internals.__telemetryUtil` at load time, and
// calls them around every outbound request. Nothing is recorded, so they do
// nothing.
(function () {
const { internals } = __bootstrap;

function updateSpanFromError() {}
function updateSpanFromRequest() {}
function updateSpanFromResponse() {}
function updateSpanFromClientResponse() {}
function updateSpanFromServerResponse() {}

internals.__telemetryUtil = {
  updateSpanFromError,
  updateSpanFromRequest,
  updateSpanFromResponse,
  updateSpanFromClientResponse,
  updateSpanFromServerResponse,
};

return internals.__telemetryUtil;
})();
