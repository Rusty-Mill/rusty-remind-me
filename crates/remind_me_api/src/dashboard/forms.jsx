// Dashboard: the modal forms -- one per write operation the API exposes.
//
// Each owns its own draft state, its own validation, and its own rendering of
// the server's refusal, so a rejected write says why in the place the user
// typed it rather than in a toast that outlives the form.

function MemoryForm({initial, onSubmit, onCancel}) {
  const [content, setContent] = useState(initial?.content||"");
  const [category, setCategory] = useState(initial?.category||"general");
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState(initial?.tags||[]);
  const categories = ["general","preference","fact","project","person","decision","observation"];

  const handleTagKey = e => {
    if ((e.key==="Enter"||e.key===",") && tagInput.trim()) {
      e.preventDefault();
      const t = tagInput.trim().toLowerCase().replace(/,/g,"");
      if (t && !tags.includes(t)) setTags([...tags, t]);
      setTagInput("");
    }
  };
  const handleSubmit = () => { if (content.trim()) onSubmit({content:content.trim(), category, tags}); };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Content"),
      React.createElement("textarea",{value:content,onChange:e=>setContent(e.target.value),rows:5,placeholder:"What should I remember?",style:{...inputSt,resize:"vertical",fontFamily:sans,lineHeight:1.6},onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Category"),
      React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
        categories.map(c => React.createElement("button",{key:c,onClick:()=>setCategory(c),style:{padding:"6px 12px",borderRadius:6,fontSize:12,fontFamily:mono,border:"1px solid "+(category===c?(theme.categoryColors[c]||theme.accent):theme.border),background:category===c?(theme.categoryColors[c]||theme.accent)+"18":"transparent",color:category===c?(theme.categoryColors[c]||theme.accent):theme.textSecondary,cursor:"pointer",transition:"all 0.15s",textTransform:"uppercase",fontWeight:category===c?600:400}}, c))
      )
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Tags"),
      tags.length>0 && React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginBottom:8}},
        tags.map(t => React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setTags(tags.filter(x=>x!==t))}))
      ),
      React.createElement("input",{value:tagInput,onChange:e=>setTagInput(e.target.value),onKeyDown:handleTagKey,placeholder:"Type a tag and press Enter\u2026",style:inputSt,onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
      React.createElement("button",{onClick:handleSubmit,disabled:!content.trim(),style:{padding:"8px 20px",borderRadius:6,border:"none",background:content.trim()?theme.accent:theme.surfaceActive,color:content.trim()?"#fff":theme.textMuted,fontSize:13,fontWeight:600,fontFamily:mono,cursor:content.trim()?"pointer":"not-allowed"}}, initial?"Save Changes":"Add Memory")
    )
  );
}

// The dashboard's remind_me_set_reminder. Submits a UTC RFC 3339 instant
// converted from whatever the browser's date picker collected locally, and
// renders the server's own rejection reason rather than a generic failure:
// "must be in the future" and "no such memory" want different fixes.
function ReminderForm({memory, onSubmit, onCancel}) {
  const [value, setValue] = useState(toLocalInputValue(memory.remind_at));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);

  const submit = async (remindAt) => {
    setBusy(true); setError(null);
    let outcome;
    try { outcome = await onSubmit(remindAt); }
    catch (e) { setBusy(false); setError("Request failed: " + e.message); return; }
    setBusy(false);
    if (!outcome) { setError("No response from the server."); return; }
    if (outcome.outcome === "rejected") { setError(outcome.reason || "The server refused that timestamp."); return; }
    if (outcome.outcome === "not_found") { setError("That memory no longer exists."); return; }
    if (outcome.error) { setError(outcome.error); return; }
    onCancel();
  };

  const handleSet = () => {
    if (!value) { setError("Pick a date and time first."); return; }
    const at = new Date(value);
    if (isNaN(at.getTime())) { setError("That is not a valid date and time."); return; }
    submit(at.toISOString());
  };

  const preview = memory.content.length > 140 ? memory.content.slice(0,140) + "…" : memory.content;

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    React.createElement("div",{style:{fontFamily:sans,fontSize:13,lineHeight:1.6,color:theme.textSecondary,padding:"10px 12px",background:theme.bg,border:"1px solid "+theme.border,borderRadius:6}}, preview),
    memory.remind_at && React.createElement("div",{style:{fontSize:12,fontFamily:mono,color:isPast(memory.remind_at)?theme.danger:theme.warning}},
      (isPast(memory.remind_at)?"Currently due ":"Currently set for ")+formatWhen(memory.remind_at)
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Remind me at"),
      React.createElement("input",{type:"datetime-local",value:value,onChange:e=>setValue(e.target.value),
        // The server refuses a past timestamp outright, so the picker steers
        // away from one rather than letting the round trip do the teaching.
        min:toLocalInputValue(new Date().toISOString()),
        style:{...inputSt,fontFamily:mono},
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}}),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},"Your local time; stored as UTC.")
    ),
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8,flexWrap:"wrap"}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
      memory.remind_at && React.createElement("button",{onClick:()=>submit(null),disabled:busy,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.danger+"40",background:"transparent",color:theme.danger,fontSize:13,fontFamily:mono,cursor:busy?"wait":"pointer"}},"Clear reminder"),
      React.createElement("button",{onClick:handleSet,disabled:busy,style:{padding:"8px 20px",borderRadius:6,border:"none",background:busy?theme.surfaceActive:theme.accent,color:busy?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:busy?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        busy && React.createElement(Icons.Loader),
        memory.remind_at ? "Update" : "Set reminder"
      )
    )
  );
}

// The dashboard's remind_me_wiki_write.
//
// `initial` null means a new page; otherwise the page being edited, whose
// title is shown but left editable — retitling writes a new page and leaves
// the old one, so the form says so rather than letting it be discovered.
function WikiPageForm({initial, onSubmit, onLoadSchema, onCancel}) {
  const [title, setTitle] = useState(initial?.title || "");
  const [content, setContent] = useState(initial?.content || "");
  const [logNote, setLogNote] = useState("");
  const [schema, setSchema] = useState("");
  const [showSchema, setShowSchema] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const retitled = !!initial && title.trim() !== initial.title;

  const handleSubmit = async () => {
    if (!title.trim() || !content.trim()) { setError("A title and a body are both required."); return; }
    setBusy(true); setError(null);
    let outcome;
    try { outcome = await onSubmit(title.trim(), content, logNote.trim()); }
    catch (e) { setBusy(false); setError("Request failed: " + e.message); return; }
    setBusy(false);
    if (outcome && outcome.error) { setError(outcome.error); return; }
    onCancel();
  };

  // Fetched on first reveal rather than on mount: most edits never open it,
  // and it is a file read on the server side.
  const loadSchema = async () => {
    if (!showSchema && !schema) setSchema(await onLoadSchema());
    setShowSchema(!showSchema);
  };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Title"),
      React.createElement("input",{value:title,onChange:e=>setTitle(e.target.value),maxLength:200,placeholder:"VLAN Setup",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}}),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},"The title's slug is the page's identity — keep it stable so [[wikilinks]] resolve.")
    ),
    retitled && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.warningSubtle,border:"1px solid "+theme.warning+"40",color:theme.warning,fontSize:12,fontFamily:mono,lineHeight:1.5}},
      "Saving under a new title writes a new page. “"+initial.title+"” will still be there, and [[links]] to it keep pointing at the old one — delete it yourself if that is what you meant."
    ),
    React.createElement("div", null,
      React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"baseline"}},
        React.createElement("label",{style:labelSt},"Body (markdown)"),
        React.createElement("button",{onClick:loadSchema,style:{background:"none",border:"none",color:theme.accent,fontSize:11,fontFamily:mono,cursor:"pointer",padding:0,marginBottom:6}}, showSchema?"Hide schema":"Maintainer schema")
      ),
      showSchema && React.createElement("pre",{style:{margin:"0 0 8px",padding:"10px 12px",borderRadius:6,background:theme.bg,border:"1px solid "+theme.border,color:theme.textSecondary,fontSize:11,fontFamily:mono,whiteSpace:"pre-wrap",maxHeight:180,overflowY:"auto"}}, schema || "Loading…"),
      React.createElement("textarea",{value:content,onChange:e=>setContent(e.target.value),rows:14,maxLength:100000,
        placeholder:"Open with a one-sentence summary; it becomes the index entry.\n\nDistil, don't paste. Cross-link with [[Other Page]].",
        style:{...inputSt,resize:"vertical",fontFamily:mono,fontSize:13,lineHeight:1.6},
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}}),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},
        content.length.toLocaleString()+" / 100,000 characters · a leading “# Title” is added if absent · saving replaces the whole body")
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Log note (optional)"),
      React.createElement("input",{value:logNote,onChange:e=>setLogNote(e.target.value),maxLength:500,placeholder:"Recorded in log.md alongside the change",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
      React.createElement("button",{onClick:handleSubmit,disabled:busy||!title.trim()||!content.trim(),style:{padding:"8px 20px",borderRadius:6,border:"none",background:(busy||!title.trim()||!content.trim())?theme.surfaceActive:theme.accent,color:(busy||!title.trim()||!content.trim())?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:busy?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        busy && React.createElement(Icons.Loader),
        initial ? "Save page" : "Create page"
      )
    )
  );
}

// The dashboard's remind_me_wiki_compile, and the one place this UI is
// deliberately a relay rather than an actor.
//
// Phase one returns a brief: the raw memories since the watermark, the current
// page index and the maintainer schema, assembled into a prompt. Synthesising
// pages from it is a judgment call and this page has no model to make one, so
// the honest affordance is "here is what is pending, copy it to Claude" plus
// the mechanical half — marking the batch integrated once the pages exist.
// Pretending a Compile button could write the pages would be the lie.
function WikiCompilePanel({onCompile, onClose}) {
  const [outcome, setOutcome] = useState(null);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState(null);
  const [copied, setCopied] = useState(false);
  const [confirmIntegrate, setConfirmIntegrate] = useState(false);

  const run = useCallback(async (markIntegrated) => {
    setBusy(true); setError(null);
    let result;
    try { result = await onCompile(markIntegrated); }
    catch (e) { setBusy(false); setError("Request failed: " + e.message); return; }
    setBusy(false);
    if (result && result.error) { setError(result.error); return; }
    setOutcome(result);
    setConfirmIntegrate(false);
  }, [onCompile]);

  useEffect(() => { run(false); }, [run]);

  const copy = () => {
    navigator.clipboard.writeText(outcome.brief);
    setCopied(true);
    setTimeout(()=>setCopied(false), 1500);
  };

  if (busy && !outcome) {
    return React.createElement("div",{style:{display:"flex",alignItems:"center",gap:8,color:theme.textSecondary,fontFamily:mono,fontSize:13,padding:"12px 0"}},
      React.createElement(Icons.Loader), " Gathering pending memories…");
  }

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),

    outcome && outcome.status === "noop" && React.createElement("div",{style:{fontSize:13,fontFamily:sans,color:theme.textSecondary,lineHeight:1.6}},
      "Nothing pending — every raw memory is already integrated into the wiki."
    ),

    outcome && outcome.status === "integrated" && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.successSubtle,border:"1px solid "+theme.success+"40",color:theme.success,fontSize:13,fontFamily:mono,lineHeight:1.5}},
      "✓ Marked "+outcome.sources_marked+" source"+(outcome.sources_marked===1?"":"s")+" integrated. The watermark moved to "+(outcome.watermark||"").slice(0,19)+"."
    ),

    outcome && outcome.status === "brief" && React.createElement(React.Fragment,null,
      React.createElement("div",{style:{fontSize:13,fontFamily:sans,color:theme.textSecondary,lineHeight:1.6}},
        React.createElement("b",{style:{color:theme.text}}, outcome.pending+" raw "+(outcome.pending===1?"memory":"memories")),
        " to synthesise. The brief below is written for Claude, not for this page — copy it into a session, let it write the pages with ",
        React.createElement("code",{style:{fontFamily:mono,fontSize:12,color:theme.text}},"remind_me_wiki_write"),
        " (or write them here yourself), then come back and mark the batch integrated."
      ),
      React.createElement("pre",{style:{margin:0,padding:"12px 14px",borderRadius:6,background:theme.bg,border:"1px solid "+theme.border,color:theme.textSecondary,fontSize:11,fontFamily:mono,whiteSpace:"pre-wrap",wordBreak:"break-word",maxHeight:300,overflowY:"auto"}}, outcome.brief),
      confirmIntegrate && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.warningSubtle,border:"1px solid "+theme.warning+"40",color:theme.warning,fontSize:12,fontFamily:mono,lineHeight:1.5}},
        "Only if the pages are written. Marking integrated moves the watermark past these "+outcome.pending+" — they will not appear in a future brief, written up or not."
      )
    ),

    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",flexWrap:"wrap",marginTop:8}},
      React.createElement("button",{onClick:onClose,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Close"),
      outcome && outcome.status === "brief" && React.createElement("button",{onClick:copy,style:{display:"flex",alignItems:"center",gap:6,padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},
        copied ? React.createElement(Icons.Check) : React.createElement(Icons.Copy), copied ? "Copied" : "Copy brief"),
      outcome && outcome.status === "brief" && React.createElement("button",{onClick:()=>confirmIntegrate?run(true):setConfirmIntegrate(true),disabled:busy,
        style:{padding:"8px 20px",borderRadius:6,border:"none",background:busy?theme.surfaceActive:(confirmIntegrate?theme.warning:theme.accent),color:busy?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:busy?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        busy && React.createElement(Icons.Loader),
        confirmIntegrate ? "Yes, mark integrated" : "Mark integrated")
    )
  );
}

// The dashboard's remind_me_save_search. Prefilled from whatever the Browse
// view is currently filtered to, since "save this search" means the one on
// screen — retyping the query that is already in the box would be the whole
// feature undone.
function SaveSearchForm({initial, onSubmit, onCancel}) {
  const [name, setName] = useState("");
  const [query, setQuery] = useState(initial?.query || "");
  const [category, setCategory] = useState(initial?.category || "");
  const [tags, setTags] = useState(initial?.tags || []);
  const [tagInput, setTagInput] = useState("");
  const [watch, setWatch] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);

  const handleTagKey = e => {
    if ((e.key==="Enter"||e.key===",") && tagInput.trim()) {
      e.preventDefault();
      const t = tagInput.trim().toLowerCase().replace(/,/g,"");
      if (t && !tags.includes(t)) setTags([...tags, t]);
      setTagInput("");
    }
  };

  const handleSubmit = async () => {
    if (!name.trim() || !query.trim()) { setError("A name and a query are both required."); return; }
    setBusy(true); setError(null);
    let saved;
    try {
      saved = await onSubmit({
        name: name.trim(),
        query: query.trim(),
        category: category.trim() || null,
        tags: tags.length ? tags : null,
        watch,
      });
    } catch (e) { setBusy(false); setError("Request failed: " + e.message); return; }
    setBusy(false);
    if (saved && saved.error) { setError(saved.error); return; }
    onCancel();
  };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Name"),
      React.createElement("input",{value:name,onChange:e=>setName(e.target.value),placeholder:"open questions",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}}),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},"Saving under a name that already exists replaces it.")
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Query"),
      React.createElement("input",{value:query,onChange:e=>setQuery(e.target.value),style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Category filter (optional)"),
      React.createElement("input",{value:category,onChange:e=>setCategory(e.target.value),placeholder:"any",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Tag filter (a match must have all of them)"),
      tags.length>0 && React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginBottom:8}},
        tags.map(t => React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setTags(tags.filter(x=>x!==t))}))
      ),
      React.createElement("input",{value:tagInput,onChange:e=>setTagInput(e.target.value),onKeyDown:handleTagKey,placeholder:"Type a tag and press Enter…",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("label", {style:{display:"flex",alignItems:"center",gap:8,fontSize:13,fontFamily:sans,color:theme.textSecondary,cursor:"pointer"}},
      React.createElement("input",{type:"checkbox",checked:watch,onChange:e=>setWatch(e.target.checked)}),
      "Watch — report matches that have not been seen before. Does not narrow what running it returns."
    ),
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
      React.createElement("button",{onClick:handleSubmit,disabled:busy||!name.trim()||!query.trim(),style:{padding:"8px 20px",borderRadius:6,border:"none",background:(busy||!name.trim()||!query.trim())?theme.surfaceActive:theme.accent,color:(busy||!name.trim()||!query.trim())?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:busy?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        busy && React.createElement(Icons.Loader),
        "Save search"
      )
    )
  );
}

function ImportForm({onComplete, onCancel}) {
  const [filePath, setFilePath] = useState("");
  const [directory, setDirectory] = useState("");
  const [mode, setMode] = useState("file"); // file or directory
  const [extractMode, setExtractMode] = useState("assistant_messages");
  const [category, setCategory] = useState("chat_import");
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState([]);
  const [importing, setImporting] = useState(false);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);

  const extractModes = [
    {value:"assistant_messages", label:"Assistant messages", desc:"Only Claude/AI responses (best for knowledge base)"},
    {value:"user_messages", label:"User messages", desc:"Only your messages"},
    {value:"all_messages", label:"All messages", desc:"Both sides, prefixed with role"},
    {value:"conversations", label:"Full conversations", desc:"Entire conversations as single memories"},
  ];

  const handleTagKey = e => {
    if ((e.key==="Enter"||e.key===",") && tagInput.trim()) {
      e.preventDefault();
      const t = tagInput.trim().toLowerCase().replace(/,/g,"");
      if (t && !tags.includes(t)) setTags([...tags, t]);
      setTagInput("");
    }
  };

  const handleImport = async () => {
    const path = mode === "file" ? filePath.trim() : directory.trim();
    if (!path) return;
    setImporting(true); setError(null); setResult(null);
    try {
      const body = { file_path: path, category, tags, extract_mode: extractMode };
      const data = await api("/import", { method: "POST", body });
      if (data.error) { setError(data.error); }
      else { setResult(data); onComplete(); }
    } catch (e) { setError("Import failed: " + e.message); }
    setImporting(false);
  };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    // Mode toggle
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Import Type"),
      React.createElement("div",{style:{display:"flex",gap:6}},
        [["file","Single File"],["directory","Directory"]].map(([v,l])=>
          React.createElement("button",{key:v,onClick:()=>setMode(v),style:{padding:"6px 14px",borderRadius:6,fontSize:12,fontFamily:mono,border:"1px solid "+(mode===v?theme.accent:theme.border),background:mode===v?theme.accentSubtle:"transparent",color:mode===v?theme.accent:theme.textSecondary,cursor:"pointer",fontWeight:mode===v?600:400}},l)
        )
      )
    ),
    // Path input
    React.createElement("div", null,
      React.createElement("label",{style:labelSt}, mode==="file" ? "File Path" : "Directory Path"),
      React.createElement("input",{
        value: mode==="file" ? filePath : directory,
        onChange: e => mode==="file" ? setFilePath(e.target.value) : setDirectory(e.target.value),
        placeholder: mode==="file"
          ? "~/Downloads/claude-export/conversations.json"
          : "~/Downloads/claude-export/",
        style: inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},
        onBlur:e=>{e.target.style.borderColor=theme.border},
      }),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},
        mode==="file"
          ? "Supports .json, .jsonl, .md, .txt files"
          : "Will scan for all supported files" + " (recursively)"
      )
    ),
    // Extract mode
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Extract Mode"),
      React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:4}},
        extractModes.map(em =>
          React.createElement("button",{key:em.value,onClick:()=>setExtractMode(em.value),style:{display:"flex",flexDirection:"column",alignItems:"flex-start",padding:"8px 12px",borderRadius:6,border:"1px solid "+(extractMode===em.value?theme.accent:theme.border),background:extractMode===em.value?theme.accentSubtle:"transparent",cursor:"pointer",transition:"all 0.15s"}},
            React.createElement("span",{style:{fontSize:13,fontFamily:mono,fontWeight:extractMode===em.value?600:400,color:extractMode===em.value?theme.accent:theme.text}}, em.label),
            React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:sans,marginTop:2}}, em.desc)
          )
        )
      )
    ),
    // Category
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Category"),
      React.createElement("input",{value:category,onChange:e=>setCategory(e.target.value),style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    // Tags
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Tags (applied to all imported memories)"),
      tags.length > 0 && React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginBottom:8}},
        tags.map(t => React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setTags(tags.filter(x=>x!==t))}))
      ),
      React.createElement("input",{value:tagInput,onChange:e=>setTagInput(e.target.value),onKeyDown:handleTagKey,placeholder:"Type a tag and press Enter\u2026",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    // Error
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),
    // Result
    result && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.successSubtle,border:"1px solid "+theme.success+"40",color:theme.success,fontSize:13,fontFamily:mono}},
      result.status === "ok"
        ? "\u2713 Imported "+result.memories_created+" memories from "+result.file
        : result.status === "skipped"
          ? "Skipped: "+result.reason + (result.file ? " ("+result.file+")" : "")
          : result.files_processed
            ? "\u2713 Processed "+result.files_processed+" files: "+result.total_memories_created+" memories created, "+result.skipped+" skipped"
            : JSON.stringify(result)
    ),
    // Actions
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Close"),
      React.createElement("button",{onClick:handleImport,disabled:importing||!(mode==="file"?filePath.trim():directory.trim()),style:{padding:"8px 20px",borderRadius:6,border:"none",background:importing||!(mode==="file"?filePath.trim():directory.trim())?theme.surfaceActive:theme.accent,color:importing||!(mode==="file"?filePath.trim():directory.trim())?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:importing?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        importing && React.createElement(Icons.Loader),
        importing ? "Importing\u2026" : "Import"
      )
    )
  );
}
