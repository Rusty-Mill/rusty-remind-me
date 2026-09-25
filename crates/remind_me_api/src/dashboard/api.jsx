// Dashboard: the fetch layer and the API-key prompt.
//
// The single place that knows the key exists, where it is stored, and what a
// 401 means. Everything else calls `api(path, opts)` and gets parsed JSON.

// --- API layer ---
// The API requires a bearer token by default. The key lives in
// ~/.remind-me/api_key on the server machine (or REMIND_ME_API_KEY).
const API_KEY_STORAGE = "remind_me_api_key";
let apiKey = "";
try { apiKey = localStorage.getItem(API_KEY_STORAGE) || ""; } catch (e) { /* storage unavailable */ }

function promptForApiKey() {
  const entered = window.prompt(
    "This dashboard's API requires a key.\n\n" +
    "Find it in ~/.remind-me/api_key on the machine running remind-me-mcp\n" +
    "(or use the value of REMIND_ME_API_KEY):"
  );
  if (entered && entered.trim()) {
    apiKey = entered.trim();
    try { localStorage.setItem(API_KEY_STORAGE, apiKey); } catch (e) { /* storage unavailable */ }
    return true;
  }
  return false;
}

async function api(path, opts = {}) {
  const url = path.startsWith("http") ? path : API + path;
  const doFetch = () => fetch(url, {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      ...(apiKey ? { "Authorization": "Bearer " + apiKey } : {}),
      ...(opts.headers || {}),
    },
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  let res = await doFetch();
  if (res.status === 401 && promptForApiKey()) {
    res = await doFetch();
  }
  return res.json();
}
