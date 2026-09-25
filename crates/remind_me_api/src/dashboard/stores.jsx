// Dashboard: the data layer -- one `use*Store` hook per API surface.
//
// Each owns its own loading/error state and exposes plain functions over
// `api()`. Components below never call `api()` directly, so which endpoint
// backs a view is answerable by reading one hook rather than grepping JSX.


// Issue #211: the builds on each side of sync. The node's comes from /health
// rather than /api/stats deliberately -- /health is unauthenticated, so it
// still shows while the API key is wrong or missing, which is one of the
// situations where you most want to know which build you are talking to. The
// hub's needs /api/versions (another machine's build isn't ours to publish
// unauthenticated), and is absent whenever sync is off or the hub can't be
// reached. Both failures are silent: a missing version renders nothing rather
// than putting an error in the chrome.
function useServerVersion() {
  const [version, setVersion] = useState("");
  const [hubVersion, setHubVersion] = useState("");
  useEffect(() => {
    let cancelled = false;
    fetch(window.location.origin + "/health")
      .then(r => r.json())
      .then(d => { if (!cancelled && d && d.version) setVersion(d.version); })
      .catch(() => {});
    api("/versions")
      .then(d => { if (!cancelled && d && d.hub) setHubVersion(d.hub); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, []);
  return { version, hubVersion };
}

function useMemoryStore() {
  const [memories, setMemories] = useState([]);
  const [stats, setStats] = useState({ total: 0, categories: {}, sources: {}, tags: {} });
  const [vitality, setVitality] = useState({ vitality_buckets: {}, active_count: 0, dormant_count: 0, vault_health_score: "0%", average_vitality: 0 });
  const [trend, setTrend] = useState({ snapshots: [] });
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async (params = {}) => {
    setLoading(true);
    try {
      const qs = new URLSearchParams();
      if (params.category) qs.set("category", params.category);
      if (params.tags && params.tags.length) qs.set("tags", params.tags.join(","));
      if (params.source) qs.set("source", params.source);
      qs.set("limit", "200");
      const data = await api("/memories?" + qs.toString());
      setMemories(data.memories || []);
    } catch (e) { console.error("refresh:", e); }
    try {
      const s = await api("/stats");
      setStats(s);
    } catch (e) { console.error("stats:", e); }
    try {
      const v = await api("/vitality");
      setVitality(v);
    } catch (e) { console.error("vitality:", e); }
    try {
      const t = await api("/analytics/trend");
      setTrend(t);
    } catch (e) { console.error("analytics trend:", e); }
    setLoading(false);
  }, []);

  const search = useCallback(async (query, category, tags) => {
    if (!query.trim()) { refresh({ category, tags }); return; }
    setLoading(true);
    try {
      const qs = new URLSearchParams({ q: query, limit: "200" });
      if (category) qs.set("category", category);
      if (tags && tags.length) qs.set("tags", tags.join(","));
      const data = await api("/memories/search?" + qs.toString());
      setMemories(data.memories || []);
    } catch (e) { console.error("search:", e); }
    setLoading(false);
  }, [refresh]);

  const add = useCallback(async (mem) => {
    await api("/memories", { method: "POST", body: mem });
    refresh();
  }, [refresh]);

  const update = useCallback(async (id, updates) => {
    await api("/memories/" + id, { method: "PUT", body: updates });
    refresh();
  }, [refresh]);

  const remove = useCallback(async (id) => {
    await api("/memories/" + id, { method: "DELETE" });
    refresh();
  }, [refresh]);

  useEffect(() => { refresh(); }, [refresh]);

  return { memories, stats, vitality, trend, loading, refresh, search, add, update, remove };
}

function useWikiStore() {
  const [pages, setPages] = useState([]);
  const [status, setStatus] = useState({ pages: 0, pending_compile: 0 });
  const [current, setCurrent] = useState(null);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api("/wiki");
      setPages(data.pages || []);
    } catch (e) { console.error("wiki pages:", e); }
    try {
      const s = await api("/wiki/status");
      setStatus(s);
    } catch (e) { console.error("wiki status:", e); }
    setLoading(false);
  }, []);

  const openPage = useCallback(async (slug) => {
    setLoading(true);
    try {
      const data = await api("/wiki/" + encodeURIComponent(slug));
      setCurrent(data.error ? null : data);
    } catch (e) { console.error("wiki page:", e); setCurrent(null); }
    setLoading(false);
  }, []);

  const search = useCallback(async (query) => {
    try {
      const data = await api("/wiki/search?" + new URLSearchParams({ q: query, limit: "20" }));
      return data.results || [];
    } catch (e) { console.error("wiki search:", e); return []; }
  }, []);

  // Create or replace a page. The slug is derived from the title server-side,
  // so retitling writes a new page rather than renaming one -- core's own
  // behaviour for remind_me_wiki_write, surfaced here rather than papered over.
  const write = useCallback(async (title, content, logNote) => {
    const outcome = await api("/wiki", { method: "POST", body: { title, content, log_note: logNote || undefined } });
    if (!outcome.error) {
      await refresh();
      await openPage(outcome.slug);
    }
    return outcome;
  }, [refresh, openPage]);

  const remove = useCallback(async (slug) => {
    const result = await api("/wiki/" + encodeURIComponent(slug), { method: "DELETE" });
    if (!result.error) { setCurrent(null); await refresh(); }
    return result;
  }, [refresh]);

  // Phase one with no arguments, phase two with mark_integrated -- the same
  // two-call shape remind_me_wiki_compile has, kept rather than smoothed over,
  // because the gap between them is where the pages actually get written.
  const compile = useCallback(async (markIntegrated) => {
    const outcome = await api("/wiki/compile", { method: "POST", body: markIntegrated ? { mark_integrated: true } : {} });
    if (markIntegrated && !outcome.error) await refresh();
    return outcome;
  }, [refresh]);

  const readSchema = useCallback(async () => {
    const data = await api("/wiki/schema");
    return data.error ? "" : (data.schema || "");
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  return { pages, status, current, setCurrent, loading, refresh, openPage, search, write, remove, compile, readSchema };
}

function useEntityStore() {
  const [entities, setEntities] = useState([]);
  const [total, setTotal] = useState(0);
  const [current, setCurrent] = useState(null); // {entity, facts, memories, total_linked_memories}
  const [related, setRelated] = useState([]); // 1-hop traversal entities, self excluded
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api("/entities?limit=100");
      setEntities(data.entities || []);
      setTotal(data.total || 0);
    } catch (e) { console.error("entities:", e); }
    setLoading(false);
  }, []);

  const openEntity = useCallback(async (name) => {
    setLoading(true);
    try {
      const profile = await api("/entity?" + new URLSearchParams({ name }));
      if (profile.error) { setCurrent(null); setRelated([]); setLoading(false); return; }
      setCurrent(profile);
      try {
        const trav = await api("/entity/traverse?" + new URLSearchParams({ name, hops: "1" }));
        setRelated(trav.error ? [] : (trav.entities || []).filter(e => e.id !== profile.entity.id));
      } catch (e) { console.error("entity traverse:", e); setRelated([]); }
    } catch (e) { console.error("entity:", e); setCurrent(null); setRelated([]); }
    setLoading(false);
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  return { entities, total, current, setCurrent, related, loading, refresh, openEntity };
}

// Reminders — the dashboard half of remind_me_set_reminder /
// remind_me_list_reminders. Both go through GET/POST /api/reminders, which
// call the same core functions the MCP tools do, so a reminder set here and
// one set by Claude are the same row validated the same way.
//
// `when` lives in the store rather than in the view because the badge in the
// header needs the overdue count no matter which window is on screen: an
// overdue reminder is one nothing was running to deliver, and it should be
// visible from any view rather than only from the one filtered to it.
function useReminderStore() {
  const [reminders, setReminders] = useState([]);
  const [when, setWhen] = useState("upcoming");
  const [overdueCount, setOverdueCount] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);

  const refresh = useCallback(async (window_) => {
    setLoading(true);
    try {
      const data = await api("/reminders?when=" + encodeURIComponent(window_) + "&limit=100");
      if (data.error) { setError(data.error); } else { setError(null); setReminders(data.memories || []); }
    } catch (e) { setError("Could not load reminders: " + e.message); }
    try {
      const overdue = await api("/reminders?when=overdue&limit=100");
      setOverdueCount((overdue.memories || []).length);
    } catch (e) { /* the badge is not worth an error banner of its own */ }
    setLoading(false);
  }, []);

  useEffect(() => { refresh(when); }, [when, refresh]);

  // Returns the raw SetReminderOutcome so a caller can tell "rejected because
  // the timestamp is in the past" from "no such memory" — the reason is the
  // useful part, and collapsing both to a boolean would throw it away.
  const set = useCallback(async (memoryId, remindAt) => {
    const outcome = await api("/reminders", { method: "POST", body: { memory_id: memoryId, remind_at: remindAt || null } });
    if (outcome.outcome === "set" || outcome.outcome === "cleared") await refresh(when);
    return outcome;
  }, [refresh, when]);

  return { reminders, when, setWhen, overdueCount, loading, error, refresh, set };
}

// Saved searches — remind_me_save_search / list / run / delete over
// GET/POST /api/saved-searches and /api/saved-searches/{name}[/run].
function useSavedSearchStore() {
  const [searches, setSearches] = useState([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api("/saved-searches");
      if (data.error) { setError(data.error); } else { setError(null); setSearches(data.saved_searches || []); }
    } catch (e) { setError("Could not load saved searches: " + e.message); }
    setLoading(false);
  }, []);

  const save = useCallback(async (input) => {
    const saved = await api("/saved-searches", { method: "POST", body: input });
    if (!saved.error) await refresh();
    return saved;
  }, [refresh]);

  const run = useCallback(async (name) => api("/saved-searches/" + encodeURIComponent(name) + "/run"), []);

  const remove = useCallback(async (name) => {
    const result = await api("/saved-searches/" + encodeURIComponent(name), { method: "DELETE" });
    if (!result.error) await refresh();
    return result;
  }, [refresh]);

  useEffect(() => { refresh(); }, [refresh]);

  return { searches, loading, error, refresh, save, run, remove };
}

// remind_me_digest and remind_me_server_status. Unlike every other store
// here, this one does *not* load on mount: a digest builds a vitality report
// and a sync status on every call, and paying for that on a page load nobody
// asked it of is the wrong default for a panel most visits never open. It
// runs when the Stats view asks it to.
function useOpsStore() {
  const [digest, setDigest] = useState(null);
  const [status, setStatus] = useState(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState(null);

  // No window parameter: the digest's own default is the one the MCP tool and
  // the scheduled digest use, and a dashboard that could disagree with them
  // about "recent" would make the same vault report two different answers.
  const run = useCallback(async () => {
    setRunning(true); setError(null);
    try {
      const d = await api("/digest");
      if (d.error) setError(d.error); else setDigest(d);
      const s = await api("/status");
      if (!s.error) setStatus(s);
    } catch (e) { setError("Could not run the digest: " + e.message); }
    setRunning(false);
  }, []);

  return { digest, status, running, error, run };
}
