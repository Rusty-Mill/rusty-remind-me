// Dashboard: the shell -- navigation, view routing, and the modal wiring that
// connects the forms to the stores.
//
// Loads last: it references every hook, component and form declared by the
// files before it. The `<script type="text/babel">` blocks that carry these
// files share one global scope and run in document order, so that order is
// load-bearing -- see `dashboard_html()` in routes.rs.

function App() {
  const store = useMemoryStore();
  const { version: serverVersion, hubVersion } = useServerVersion();
  const wikiStore = useWikiStore();
  const entityStore = useEntityStore();
  const reminderStore = useReminderStore();
  const savedSearchStore = useSavedSearchStore();
  const ops = useOpsStore();
  const [view, setView] = useState("browse");
  const [searchQuery, setSearchQuery] = useState("");
  const [filterCategory, setFilterCategory] = useState("");
  const [filterTags, setFilterTags] = useState([]);
  const [showAddModal, setShowAddModal] = useState(false);
  const [showImportModal, setShowImportModal] = useState(false);
  const [editMemory, setEditMemory] = useState(null);
  const [deleteConfirm, setDeleteConfirm] = useState(null);
  const [expandedIds, setExpandedIds] = useState(new Set());
  const [wikiQuery, setWikiQuery] = useState("");
  const [wikiSearchResults, setWikiSearchResults] = useState(null);
  const [entityQuery, setEntityQuery] = useState("");
  const [remindMemory, setRemindMemory] = useState(null);
  // null when closed; otherwise the {query, category, tags} the form opens
  // prefilled with -- Browse passes what is on screen, the Searches view
  // passes a blank set.
  const [showSaveSearch, setShowSaveSearch] = useState(null);
  const [deleteSearchConfirm, setDeleteSearchConfirm] = useState(null);
  const [savedSearchRun, setSavedSearchRun] = useState(null); // {name, query, count, results}
  const [runningSearch, setRunningSearch] = useState("");
  // null when closed; {page: null} for a new page, {page: <the page>} to edit.
  const [wikiEdit, setWikiEdit] = useState(null);
  const [wikiDeleteConfirm, setWikiDeleteConfirm] = useState(null);
  const [showCompile, setShowCompile] = useState(false);
  const searchRef = useRef(null);
  const debounceRef = useRef(null);
  const wikiDebounceRef = useRef(null);

  useEffect(() => {
    const h = e => { if ((e.metaKey||e.ctrlKey)&&e.key==="k") { e.preventDefault(); searchRef.current?.focus(); } };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, []);

  // Debounced search
  useEffect(() => {
    clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      if (searchQuery.trim()) {
        store.search(searchQuery, filterCategory||null, filterTags.length?filterTags:null);
      } else {
        store.refresh({ category: filterCategory||undefined, tags: filterTags.length?filterTags:undefined });
      }
    }, 250);
  }, [searchQuery, filterCategory, filterTags]);

  // Debounced wiki search — a filter over the page list, mirroring the
  // Browse search box; null means "show every page" (no query typed).
  useEffect(() => {
    clearTimeout(wikiDebounceRef.current);
    wikiDebounceRef.current = setTimeout(async () => {
      if (wikiQuery.trim()) {
        setWikiSearchResults(await wikiStore.search(wikiQuery));
      } else {
        setWikiSearchResults(null);
      }
    }, 250);
  }, [wikiQuery]);

  const handleAdd = async data => { await store.add(data); setShowAddModal(false); };
  const handleEdit = async data => { if (editMemory) { await store.update(editMemory.id, data); setEditMemory(null); } };
  const handleDelete = async id => { await store.remove(id); setDeleteConfirm(null); };
  const toggleExpand = id => setExpandedIds(prev => { const n=new Set(prev); n.has(id)?n.delete(id):n.add(id); return n; });
  const handleTagClick = tag => { if (!filterTags.includes(tag)) setFilterTags([...filterTags, tag]); };
  const handleWikiNavigate = slug => { wikiStore.openPage(slug); setWikiQuery(""); setWikiSearchResults(null); };
  const handleEntityNavigate = name => { entityStore.openEntity(name); setEntityQuery(""); };

  // Setting a reminder changes the memory's own row, so the browse list is
  // refreshed alongside the reminder list -- otherwise a card would keep
  // showing the old badge until something else happened to reload it.
  const handleSetReminder = async (remindAt) => {
    const outcome = await reminderStore.set(remindMemory.id, remindAt);
    if (outcome.outcome === "set" || outcome.outcome === "cleared") store.refresh();
    return outcome;
  };
  const handleWikiWrite = async (title, content, logNote) => wikiStore.write(title, content, logNote);
  const handleWikiDelete = async slug => {
    await wikiStore.remove(slug);
    setWikiDeleteConfirm(null);
  };
  const handleSaveSearch = async input => savedSearchStore.save(input);
  const handleRunSavedSearch = async name => {
    setRunningSearch(name);
    const result = await savedSearchStore.run(name);
    setRunningSearch("");
    setSavedSearchRun(result && result.error ? { name, error: result.error } : result);
  };
  const handleDeleteSavedSearch = async name => {
    await savedSearchStore.remove(name);
    setDeleteSearchConfirm(null);
    if (savedSearchRun && savedSearchRun.name === name) setSavedSearchRun(null);
  };

  const stats = store.stats;
  const vitality = store.vitality;
  const trend = store.trend;
  const allCategories = Object.keys(stats.categories||{});
  const allTags = Object.keys(stats.tags||{}).sort((a,b)=>(stats.tags[b]||0)-(stats.tags[a]||0));
  const entityQueryLower = entityQuery.trim().toLowerCase();
  const filteredEntities = entityQueryLower
    ? entityStore.entities.filter(e =>
        e.name.toLowerCase().includes(entityQueryLower) ||
        (e.aliases||[]).some(a => a.toLowerCase().includes(entityQueryLower)))
    : entityStore.entities;

  return React.createElement("div", {style:{minHeight:"100vh",background:theme.bg,color:theme.text,fontFamily:sans}},
    // Mobile responsive fixes (issue #199 audit). Kept as a small embedded
    // stylesheet -- like the pre-existing @keyframes rule below -- rather
    // than reworking every inline style object, since this codebase's
    // established style is React.createElement + inline style props with
    // no className/CSS-in-JS setup. !important is needed only on <aside>,
    // whose width/height/position are already set inline (per-view, three
    // call sites below) -- inline styles otherwise win over embedded
    // stylesheet rules of equal specificity for the same property.
    React.createElement("style",null,`
      @keyframes spin{to{transform:rotate(360deg)}}
      @media (max-width: 680px) {
        header { flex-wrap: wrap; row-gap: 8px; }
        [data-shell-body] { flex-direction: column; }
        aside {
          width: 100% !important;
          max-width: none !important;
          height: auto !important;
          max-height: 40vh !important;
          position: static !important;
          border-right: none !important;
          border-bottom: 1px solid `+theme.border+`;
        }
      }
    `),
    // Header
    React.createElement("header", {style:{borderBottom:"1px solid "+theme.border,padding:"16px 24px",display:"flex",alignItems:"center",justifyContent:"space-between",position:"sticky",top:0,zIndex:100,background:theme.bg+"e6",backdropFilter:"blur(12px)"}},
      React.createElement("div",{style:{display:"flex",alignItems:"center",gap:10}},
        React.createElement("div",{style:{width:36,height:36,borderRadius:8,background:"linear-gradient(135deg,"+theme.accent+",#a855f7)",display:"flex",alignItems:"center",justifyContent:"center"}}, React.createElement(Icons.Brain)),
        React.createElement("div",null,
          React.createElement("h1",{style:{margin:0,fontSize:18,fontWeight:700,fontFamily:sans,letterSpacing:"-0.02em"}},"Remind Me"),
          React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, (stats.total||0)+" memories \u00b7 "+((stats.db_path||"").replace(/.*\//,"~/"))+(serverVersion?" \u00b7 v"+serverVersion:"")+(hubVersion?" \u00b7 hub v"+hubVersion:"")),
        )
      ),
      React.createElement("div",{style:{display:"flex",alignItems:"center",gap:8,flexWrap:"wrap"}},
        store.loading && React.createElement("span",{style:{color:theme.textMuted}}, React.createElement(Icons.Loader)),
        React.createElement("div",{style:{display:"flex",background:theme.surface,borderRadius:6,border:"1px solid "+theme.border,overflow:"hidden"}},
          [["browse","Browse"],["stats","Stats"],["wiki","Wiki"],["entities","Entities"],["reminders","Reminders"],["searches","Searches"]].map(([v,l])=>React.createElement("button",{key:v,onClick:()=>setView(v),style:{padding:"10px 14px",minHeight:40,border:"none",fontSize:12,fontFamily:mono,fontWeight:500,cursor:"pointer",background:view===v?theme.accent:"transparent",color:view===v?"#fff":theme.textSecondary,transition:"all 0.15s"}},
            l,
            v==="wiki" && wikiStore.status.pending_compile>0 && React.createElement("span",{style:{marginLeft:6,padding:"1px 6px",borderRadius:8,background:view===v?"rgba(255,255,255,0.25)":theme.warningSubtle,color:view===v?"#fff":theme.warning,fontSize:10,fontWeight:700}}, wikiStore.status.pending_compile),
            // Overdue means a reminder came due with nothing running to
            // deliver it, so it is worth surfacing from every view rather
            // than only from the one already filtered to it.
            v==="reminders" && reminderStore.overdueCount>0 && React.createElement("span",{style:{marginLeft:6,padding:"1px 6px",borderRadius:8,background:view===v?"rgba(255,255,255,0.25)":theme.dangerSubtle,color:view===v?"#fff":theme.danger,fontSize:10,fontWeight:700}}, reminderStore.overdueCount)
          ))
        ),
        React.createElement("button",{onClick:()=>setShowImportModal(true),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 14px",minHeight:40,borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontWeight:500,fontFamily:mono,cursor:"pointer",transition:"all 0.15s"},onMouseEnter:e=>{e.currentTarget.style.borderColor=theme.accent;e.currentTarget.style.color=theme.text},onMouseLeave:e=>{e.currentTarget.style.borderColor=theme.border;e.currentTarget.style.color=theme.textSecondary}}, React.createElement(Icons.Upload), " Import"),
        React.createElement("button",{onClick:()=>setShowAddModal(true),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 14px",minHeight:40,borderRadius:6,border:"none",background:theme.accent,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}}, React.createElement(Icons.Plus), " Add")
      )
    ),
    // Body
    React.createElement("div",{"data-shell-body":"", style:{display:"flex",maxWidth:1200,margin:"0 auto"}},
      // Sidebar
      view==="browse" && React.createElement("aside",{style:{width:220,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{marginBottom:20}},
          React.createElement("div",{style:{...labelSt,marginBottom:10}},"Categories"),
          React.createElement("button",{onClick:()=>setFilterCategory(""),style:{display:"block",width:"100%",textAlign:"left",padding:"6px 10px",borderRadius:5,border:"none",background:!filterCategory?theme.accentSubtle:"transparent",color:!filterCategory?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:!filterCategory?600:400,marginBottom:2}}, "All ("+(stats.total||0)+")"),
          allCategories.map(cat=>React.createElement("button",{key:cat,onClick:()=>setFilterCategory(filterCategory===cat?"":cat),style:{display:"flex",alignItems:"center",justifyContent:"space-between",width:"100%",textAlign:"left",padding:"6px 10px",borderRadius:5,border:"none",background:filterCategory===cat?(theme.categoryColors[cat]||theme.accent)+"18":"transparent",color:filterCategory===cat?(theme.categoryColors[cat]||theme.accent):theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:filterCategory===cat?600:400,marginBottom:2}},
            React.createElement("span",null,cat),
            React.createElement("span",{style:{fontSize:11,fontFamily:mono,opacity:0.7}}, stats.categories[cat])
          ))
        ),
        React.createElement("div",null,
          React.createElement("div",{style:{...labelSt,marginBottom:10}},"Popular Tags"),
          React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4}},
            allTags.slice(0,15).map(t=>React.createElement(TagPill,{key:t,tag:t,onClick:()=>handleTagClick(t)}))
          )
        )
      ),
      view==="wiki" && React.createElement("aside",{style:{width:240,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{position:"relative",marginBottom:16}},
          React.createElement("div",{style:{position:"absolute",left:10,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
          React.createElement("input",{value:wikiQuery,onChange:e=>setWikiQuery(e.target.value),placeholder:"Search wiki…",style:{...inputSt,paddingLeft:32,fontSize:13,padding:"7px 10px 7px 32px"}})
        ),
        React.createElement("button",{onClick:()=>setWikiEdit({page:null}),
          style:{display:"flex",alignItems:"center",justifyContent:"center",gap:6,width:"100%",padding:"8px 12px",minHeight:40,borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",marginBottom:12},
          onMouseEnter:e=>{e.currentTarget.style.borderColor=theme.accent;e.currentTarget.style.color=theme.text},
          onMouseLeave:e=>{e.currentTarget.style.borderColor=theme.border;e.currentTarget.style.color=theme.textSecondary}},
          React.createElement(Icons.Plus), " New page"),
        // The badge became a button: it was already the only place the count
        // appeared, and "N memories not yet compiled" with nothing to press
        // was a notification about a job you had to leave the page to start.
        wikiStore.status.pending_compile>0 && React.createElement("button",{onClick:()=>setShowCompile(true),
          style:{display:"block",width:"100%",textAlign:"left",padding:"8px 10px",borderRadius:5,border:"1px solid "+theme.warning+"40",background:theme.warningSubtle,color:theme.warning,fontSize:11,fontFamily:mono,cursor:"pointer",marginBottom:14,lineHeight:1.5}},
          wikiStore.status.pending_compile+" raw "+(wikiStore.status.pending_compile===1?"memory":"memories")+" not yet compiled",
          React.createElement("span",{style:{display:"block",opacity:0.75,marginTop:2}},"Compile →")
        ),
        React.createElement("div",{style:{...labelSt,marginBottom:10}}, (wikiSearchResults?wikiSearchResults.length:wikiStore.pages.length)+" page(s)"),
        React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:2}},
          (wikiSearchResults || wikiStore.pages).map(p=>React.createElement("button",{
            key:p.slug, onClick:()=>handleWikiNavigate(p.slug),
            style:{display:"block",width:"100%",textAlign:"left",padding:"7px 10px",borderRadius:5,border:"none",background:wikiStore.current&&wikiStore.current.slug===p.slug?theme.accentSubtle:"transparent",color:wikiStore.current&&wikiStore.current.slug===p.slug?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:wikiStore.current&&wikiStore.current.slug===p.slug?600:400},
          },
            React.createElement("div",null, p.title),
            p.summary && React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:sans,marginTop:1,overflow:"hidden",textOverflow:"ellipsis",whiteSpace:"nowrap"}}, p.snippet ? React.createElement("span",{dangerouslySetInnerHTML:{__html:p.snippet.replace(/\[/g,"<b>").replace(/\]/g,"</b>")}}) : p.summary)
          )),
          !wikiStore.loading && (wikiSearchResults||wikiStore.pages).length===0 && React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:sans,padding:"8px 10px"}}, wikiQuery ? "No matches." : "The wiki is empty. Ask Claude to run remind_me_wiki_compile.")
        )
      ),
      view==="entities" && React.createElement("aside",{style:{width:240,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{position:"relative",marginBottom:16}},
          React.createElement("div",{style:{position:"absolute",left:10,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
          React.createElement("input",{value:entityQuery,onChange:e=>setEntityQuery(e.target.value),placeholder:"Filter entities…",style:{...inputSt,paddingLeft:32,fontSize:13,padding:"7px 10px 7px 32px"}})
        ),
        React.createElement("div",{style:{...labelSt,marginBottom:10}}, filteredEntities.length+" of "+entityStore.total+" entit"+(entityStore.total===1?"y":"ies")),
        React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:2}},
          filteredEntities.map(e=>React.createElement("button",{
            key:e.id, onClick:()=>handleEntityNavigate(e.name),
            style:{display:"flex",alignItems:"center",justifyContent:"space-between",width:"100%",textAlign:"left",padding:"7px 10px",borderRadius:5,border:"none",background:entityStore.current&&entityStore.current.entity.id===e.id?theme.accentSubtle:"transparent",color:entityStore.current&&entityStore.current.entity.id===e.id?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:entityStore.current&&entityStore.current.entity.id===e.id?600:400},
          },
            React.createElement("span",null, e.name),
            React.createElement("span",{style:{fontSize:11,fontFamily:mono,opacity:0.7,flexShrink:0,marginLeft:8}}, e.mention_count)
          )),
          !entityStore.loading && filteredEntities.length===0 && React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:sans,padding:"8px 10px"}},
            entityQuery ? "No matches." : "No entities yet. They're created automatically when Claude extracts facts via remind_me_decompose or remind_me_annotate."
          )
        )
      ),
      // Main
      React.createElement("main",{style:{flex:1,padding:"20px 24px",minWidth:0}},
        view==="browse" ? React.createElement(React.Fragment,null,
          // Search
          React.createElement("div",{style:{display:"flex",gap:8,alignItems:"stretch",marginBottom:16,flexWrap:"wrap"}},
            React.createElement("div",{style:{position:"relative",flex:"1 1 240px",minWidth:0}},
              React.createElement("div",{style:{position:"absolute",left:12,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
              React.createElement("input",{ref:searchRef,value:searchQuery,onChange:e=>setSearchQuery(e.target.value),placeholder:"Search memories\u2026 (\u2318K)",style:{...inputSt,paddingLeft:36,background:theme.surface,fontSize:15},onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
            ),
            // Disabled with an empty box rather than hidden: a control that
            // vanishes is one a reader has to rediscover, and there is
            // nothing to save until something has been typed.
            React.createElement("button",{onClick:()=>setShowSaveSearch({query:searchQuery,category:filterCategory,tags:filterTags}),disabled:!searchQuery.trim(),title:searchQuery.trim()?"Save this query and its filters":"Type a query first",
              style:{display:"flex",alignItems:"center",gap:6,padding:"0 14px",minHeight:44,borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:searchQuery.trim()?theme.textSecondary:theme.textMuted,fontSize:13,fontFamily:mono,cursor:searchQuery.trim()?"pointer":"not-allowed",flexShrink:0}},
              React.createElement(Icons.Bookmark), " Save search")
          ),
          // Active tag filters
          filterTags.length>0 && React.createElement("div",{style:{display:"flex",alignItems:"center",gap:6,marginBottom:12,flexWrap:"wrap"}},
            React.createElement("span",{style:{fontSize:12,color:theme.textMuted,fontFamily:mono}},"Filtered by:"),
            filterTags.map(t=>React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setFilterTags(filterTags.filter(x=>x!==t))})),
            React.createElement("button",{onClick:()=>setFilterTags([]),style:{background:"none",border:"none",color:theme.accent,fontSize:12,fontFamily:mono,cursor:"pointer"}},"Clear all")
          ),
          React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:mono,marginBottom:12}}, store.memories.length+" "+(store.memories.length===1?"memory":"memories")+(searchQuery||filterCategory||filterTags.length?" matching filters":"")),
          // Cards
          React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:10}},
            store.memories.map(m=>React.createElement(MemoryCard,{key:m.id,memory:m,onEdit:setEditMemory,onDelete:setDeleteConfirm,onTagClick:handleTagClick,onRemind:setRemindMemory,expanded:expandedIds.has(m.id),onToggle:()=>toggleExpand(m.id)})),
            store.memories.length===0 && !store.loading && React.createElement("div",{style:{textAlign:"center",padding:"60px 20px",color:theme.textMuted}},
              React.createElement("div",{style:{fontSize:40,marginBottom:12}},"\u2205"),
              React.createElement("div",{style:{fontSize:15,marginBottom:6}},"No memories found"),
              React.createElement("div",{style:{fontSize:13}},"Try adjusting your search or filters")
            )
          )
        ) :
        view==="wiki" ?
        // Wiki view
        (wikiStore.current ? React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"center",gap:8,marginBottom:12,flexWrap:"wrap"}},
            React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, "Updated "+(wikiStore.current.updated_at||"").replace("T"," ").slice(0,16)),
            React.createElement("div",{style:{display:"flex",alignItems:"center",gap:4}},
              React.createElement("button",{onClick:()=>setWikiEdit({page:wikiStore.current}),title:"Edit page",style:iconBtn}, React.createElement(Icons.Edit)),
              React.createElement("button",{onClick:()=>setWikiDeleteConfirm(wikiStore.current),title:"Delete page",style:{...iconBtn,color:theme.danger}}, React.createElement(Icons.Trash)),
              React.createElement("button",{onClick:()=>wikiStore.setCurrent(null),style:{background:"none",border:"none",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",display:"flex",alignItems:"center",gap:4,padding:"0 6px"}}, React.createElement(Icons.X), " Close")
            )
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"20px 24px"}},
            React.createElement(WikiPageBody,{content:wikiStore.current.content, onNavigate:handleWikiNavigate}),
            React.createElement(WikiLinkList,{label:"Links",items:wikiStore.current.links,onNavigate:handleWikiNavigate}),
            React.createElement(WikiLinkList,{label:"Backlinks",items:wikiStore.current.backlinks,onNavigate:handleWikiNavigate})
          )
        ) : React.createElement("div",{style:{textAlign:"center",padding:"80px 20px",color:theme.textMuted}},
          React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Book)),
          React.createElement("div",{style:{fontSize:15,marginBottom:6}}, wikiStore.pages.length===0 ? "The wiki is empty" : "Select a page"),
          React.createElement("div",{style:{fontSize:13,marginBottom:16}}, wikiStore.pages.length===0 ? "Compile one from your memories, or write the first page by hand." : "Pick a page from the list on the left."),
          wikiStore.pages.length===0 && React.createElement("div",{style:{display:"flex",gap:8,justifyContent:"center",flexWrap:"wrap"}},
            React.createElement("button",{onClick:()=>setShowCompile(true),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 16px",minHeight:40,borderRadius:6,border:"none",background:theme.accent,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}}, React.createElement(Icons.Play), " Compile"),
            React.createElement("button",{onClick:()=>setWikiEdit({page:null}),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 16px",minHeight:40,borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}}, React.createElement(Icons.Plus), " New page")
          )
        )) :
        view==="entities" ?
        // Entity detail view
        (entityStore.current ? React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",marginBottom:12}},
            React.createElement("div",null,
              React.createElement("h2",{style:{margin:0,fontFamily:sans,fontWeight:700,fontSize:20,letterSpacing:"-0.02em"}}, entityStore.current.entity.name),
              entityStore.current.entity.kind && React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, entityStore.current.entity.kind)
            ),
            React.createElement("button",{onClick:()=>entityStore.setCurrent(null),style:{background:"none",border:"none",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",display:"flex",alignItems:"center",gap:4}}, React.createElement(Icons.X), " Close")
          ),
          (entityStore.current.entity.aliases||[]).length>0 && React.createElement("div",{style:{marginBottom:12,display:"flex",flexWrap:"wrap",gap:4}},
            entityStore.current.entity.aliases.map(a=>React.createElement(TagPill,{key:a,tag:a}))
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"20px 24px"}},
            React.createElement("div",{style:labelSt}, "Facts ("+entityStore.current.facts.length+")"),
            entityStore.current.facts.length===0 ?
              React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans,marginBottom:16}},"No structured facts yet.") :
              React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:8,marginBottom:16}},
                entityStore.current.facts.map(f=>React.createElement("div",{key:f.id,style:{fontFamily:mono,fontSize:13,color:theme.text,padding:"8px 10px",background:theme.surfaceActive,borderRadius:5}},
                  f.subject && f.predicate && f.object ?
                    React.createElement("span",null, React.createElement("b",null,f.subject)," ",f.predicate," ",React.createElement("b",null,f.object)) :
                    f.content
                ))
              ),
            React.createElement("div",{style:labelSt}, "Linked Memories ("+entityStore.current.total_linked_memories+")"),
            entityStore.current.memories.length===0 ?
              React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans}},"No linked memories.") :
              React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:8}},
                entityStore.current.memories.map(m=>React.createElement("div",{key:m.id,style:{fontSize:13,fontFamily:sans,color:theme.text,padding:"8px 10px",background:theme.surfaceActive,borderRadius:5,lineHeight:1.5}},
                  React.createElement(CategoryBadge,{category:m.category}),
                  React.createElement("div",{style:{marginTop:4}}, m.content_snippet)
                ))
              ),
            React.createElement(EntityLinkList,{label:"Related Entities",items:entityStore.related,onNavigate:handleEntityNavigate})
          )
        ) : React.createElement("div",{style:{textAlign:"center",padding:"80px 20px",color:theme.textMuted}},
          React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Brain)),
          React.createElement("div",{style:{fontSize:15,marginBottom:6}}, entityStore.entities.length===0 ? "No entities yet" : "Select an entity"),
          React.createElement("div",{style:{fontSize:13}}, entityStore.entities.length===0 ? "Entities are created automatically when Claude extracts facts via remind_me_decompose or remind_me_annotate." : "Pick an entity from the list on the left.")
        )) :
        view==="reminders" ?
        // Reminders view — remind_me_list_reminders, with the set/clear half
        // reachable from every card's clock button.
        React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"center",gap:12,marginBottom:16,flexWrap:"wrap"}},
            React.createElement("h2",{style:{fontFamily:sans,fontWeight:700,fontSize:22,margin:0,letterSpacing:"-0.02em"}},"Reminders"),
            React.createElement("div",{style:{display:"flex",background:theme.surface,borderRadius:6,border:"1px solid "+theme.border,overflow:"hidden"}},
              [["upcoming","Upcoming"],["overdue","Overdue"],["all","All"]].map(([w,l])=>React.createElement("button",{key:w,onClick:()=>reminderStore.setWhen(w),
                style:{padding:"8px 14px",minHeight:40,border:"none",fontSize:12,fontFamily:mono,fontWeight:500,cursor:"pointer",background:reminderStore.when===w?theme.accent:"transparent",color:reminderStore.when===w?"#fff":theme.textSecondary}}, l))
            )
          ),
          React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:mono,marginBottom:12,lineHeight:1.6}},
            reminderStore.when==="upcoming" ? "Set and still in the future."
              : reminderStore.when==="overdue" ? "Came due and was never delivered — typically because nothing was running when it fired."
              : "Upcoming and overdue together. A delivered reminder drops out of every window."
          ),
          reminderStore.error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono,marginBottom:12}}, reminderStore.error),
          React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:10}},
            reminderStore.reminders.map(m=>React.createElement(MemoryCard,{key:m.id,memory:m,onEdit:setEditMemory,onDelete:setDeleteConfirm,onTagClick:t=>{setFilterTags([t]);setView("browse")},onRemind:setRemindMemory,expanded:expandedIds.has(m.id),onToggle:()=>toggleExpand(m.id)})),
            reminderStore.reminders.length===0 && !reminderStore.loading && React.createElement("div",{style:{textAlign:"center",padding:"60px 20px",color:theme.textMuted}},
              React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Clock)),
              React.createElement("div",{style:{fontSize:15,marginBottom:6}}, reminderStore.when==="overdue" ? "Nothing overdue" : "No reminders set"),
              React.createElement("div",{style:{fontSize:13}},"Open any memory's clock button in Browse to schedule one.")
            )
          )
        ) :
        view==="searches" ?
        // Saved searches view — remind_me_save_search / list / run / delete.
        React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"center",gap:12,marginBottom:16,flexWrap:"wrap"}},
            React.createElement("h2",{style:{fontFamily:sans,fontWeight:700,fontSize:22,margin:0,letterSpacing:"-0.02em"}},"Saved Searches"),
            React.createElement("button",{onClick:()=>setShowSaveSearch({query:"",category:"",tags:[]}),
              style:{display:"flex",alignItems:"center",gap:6,padding:"8px 14px",minHeight:40,borderRadius:6,border:"none",background:theme.accent,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}},
              React.createElement(Icons.Plus), " New")
          ),
          savedSearchStore.error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono,marginBottom:12}}, savedSearchStore.error),
          React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:10}},
            savedSearchStore.searches.map(s=>React.createElement("div",{key:s.id,style:{background:theme.surface,border:"1px solid "+((savedSearchRun&&savedSearchRun.name===s.name)?theme.accent+"60":theme.border),borderRadius:8,padding:"14px 16px"}},
              React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",gap:8,flexWrap:"wrap"}},
                React.createElement("div",{style:{minWidth:0}},
                  React.createElement("div",{style:{display:"flex",alignItems:"center",gap:8,flexWrap:"wrap"}},
                    React.createElement("span",{style:{fontFamily:sans,fontSize:15,fontWeight:600,color:theme.text}}, s.name),
                    s.watch && React.createElement("span",{style:{padding:"1px 7px",borderRadius:3,fontSize:10,fontWeight:700,fontFamily:mono,background:theme.successSubtle,color:theme.success,textTransform:"uppercase",letterSpacing:"0.06em"}},"watch")
                  ),
                  React.createElement("code",{style:{display:"block",fontFamily:mono,fontSize:12,color:theme.textSecondary,marginTop:4,wordBreak:"break-word"}}, s.query),
                  React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginTop:6,alignItems:"center"}},
                    s.filters && s.filters.category && React.createElement(CategoryBadge,{category:s.filters.category}),
                    ((s.filters && s.filters.tags) || []).map(t=>React.createElement(TagPill,{key:t,tag:t}))
                  )
                ),
                React.createElement("div",{style:{display:"flex",gap:4,flexShrink:0}},
                  React.createElement("button",{onClick:()=>handleRunSavedSearch(s.name),title:"Run",style:{...iconBtn,color:theme.accent}},
                    runningSearch===s.name ? React.createElement(Icons.Loader) : React.createElement(Icons.Play)),
                  React.createElement("button",{onClick:()=>setDeleteSearchConfirm(s.name),title:"Delete",style:{...iconBtn,color:theme.danger}}, React.createElement(Icons.Trash))
                )
              )
            )),
            savedSearchStore.searches.length===0 && !savedSearchStore.loading && React.createElement("div",{style:{textAlign:"center",padding:"60px 20px",color:theme.textMuted}},
              React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Bookmark)),
              React.createElement("div",{style:{fontSize:15,marginBottom:6}},"No saved searches yet"),
              React.createElement("div",{style:{fontSize:13}},"Search in Browse, then use “Save search” to keep the query and its filters.")
            )
          ),
          // Results of the last run, below the list rather than in a modal:
          // a saved search is usually run to read the matches, and a dialog
          // over the list would put them behind a dismiss.
          savedSearchRun && React.createElement("div",{style:{marginTop:24}},
            React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"baseline",marginBottom:12,gap:8,flexWrap:"wrap"}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,textTransform:"uppercase",letterSpacing:"0.04em",margin:0}},
                savedSearchRun.error ? "Run failed" : (savedSearchRun.count||0)+" match"+((savedSearchRun.count||0)===1?"":"es")+" for “"+savedSearchRun.name+"”"),
              React.createElement("button",{onClick:()=>setSavedSearchRun(null),style:{background:"none",border:"none",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",display:"flex",alignItems:"center",gap:4}}, React.createElement(Icons.X), " Close")
            ),
            savedSearchRun.error
              ? React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, savedSearchRun.error)
              : React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:10}},
                  (savedSearchRun.results||[]).map(r=>React.createElement(MemoryCard,{key:r.memory.id,memory:r.memory,onEdit:setEditMemory,onDelete:setDeleteConfirm,onTagClick:t=>{setFilterTags([t]);setView("browse")},onRemind:setRemindMemory,expanded:expandedIds.has(r.memory.id),onToggle:()=>toggleExpand(r.memory.id)})),
                  (savedSearchRun.results||[]).length===0 && React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans,padding:"8px 2px"}},"Nothing matches this search right now.")
                )
          )
        ) :
        // Stats view
        React.createElement("div",null,
          React.createElement("h2",{style:{fontFamily:sans,fontWeight:700,fontSize:22,marginBottom:20,letterSpacing:"-0.02em"}},"Memory Statistics"),
          React.createElement("div",{style:{display:"flex",gap:12,marginBottom:24,flexWrap:"wrap"}},
            React.createElement(StatCard,{label:"Total Memories",value:stats.total||0,color:theme.accent,icon:React.createElement(Icons.Database)}),
            React.createElement(StatCard,{label:"Categories",value:Object.keys(stats.categories||{}).length,color:"#22c55e",icon:React.createElement(Icons.Chart)}),
            React.createElement(StatCard,{label:"Unique Tags",value:Object.keys(stats.tags||{}).length,color:"#f59e0b",icon:React.createElement(Icons.Tag)}),
            React.createElement(StatCard,{label:"Sources",value:Object.keys(stats.sources||{}).length,color:"#06b6d4",icon:React.createElement(Icons.Upload)})
          ),
          // auto-fit/minmax rather than a fixed "1fr 1fr": collapses to one
          // column once a column would drop under 260px (e.g. narrow/phone
          // viewports), which BarChart needs -- its 90px fixed label plus
          // the bar track otherwise gets crushed to near-illegible widths
          // in a forced two-up grid (issue #199 mobile audit).
          React.createElement("div",{style:{display:"grid",gridTemplateColumns:"repeat(auto-fit, minmax(260px, 1fr))",gap:16}},
            React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"By Category"),
              React.createElement(BarChart,{data:stats.categories||{},colorMap:theme.categoryColors})
            ),
            React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"By Source"),
              React.createElement(BarChart,{data:stats.sources||{},colorMap:{manual:theme.accent,chat_import:"#64748b"}})
            )
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"baseline",marginBottom:16}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,textTransform:"uppercase",letterSpacing:"0.04em"}},"Vitality Distribution"),
              React.createElement("span",{style:{fontFamily:mono,fontSize:12,color:theme.textMuted}}, "Vault health "+(vitality.vault_health_score||"0%")+" · "+(vitality.active_count||0)+" active · "+(vitality.dormant_count||0)+" dormant")
            ),
            React.createElement(BarChart,{data:vitality.vitality_buckets||{},preserveOrder:true,colorMap:{"0.00-0.05":theme.danger,"0.05-0.25":"#f59e0b","0.25-0.50":"#eab308","0.50-0.75":"#84cc16","0.75+":"#22c55e"}})
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"baseline",marginBottom:16}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,textTransform:"uppercase",letterSpacing:"0.04em"}},"Vault Trend"),
              React.createElement("span",{style:{fontFamily:mono,fontSize:12,color:theme.textMuted}}, (trend.snapshots||[]).length+" daily snapshot"+((trend.snapshots||[]).length===1?"":"s"))
            ),
            React.createElement(TrendChart,{data:trend.snapshots||[],valueKey:"total_memories",color:theme.accent})
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"Top Tags"),
            React.createElement(BarChart,{data:Object.fromEntries(Object.entries(stats.tags||{}).sort((a,b)=>b[1]-a[1]).slice(0,10))})
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:12,textTransform:"uppercase",letterSpacing:"0.04em"}},"Server Info"),
            React.createElement("div",{style:{fontFamily:mono,fontSize:13,color:theme.textSecondary,lineHeight:2}},
              // wordBreak so an unusually long db path (deep home dirs,
              // Windows-style paths) wraps instead of overflowing the
              // panel on narrow viewports -- the other <code> values here
              // are all short and fixed-length so don't need it.
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Database: "), React.createElement("code",{style:{color:theme.text,wordBreak:"break-all"}}, stats.db_path||"~/.remind-me/memory.db")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Size: "), React.createElement("code",{style:{color:theme.text}}, (stats.db_size_mb||0)+" MB")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Search engine: "), React.createElement("code",{style:{color:theme.text}}, "SQLite FTS5")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"API: "), React.createElement("code",{style:{color:theme.text}}, window.location.origin))
            )
          ),
          // remind_me_digest and remind_me_server_status. Run on demand, not
          // on page load: a digest builds a vitality report and a sync status
          // every time, and most visits to this view never look at it.
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"center",gap:12,marginBottom:16,flexWrap:"wrap"}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,textTransform:"uppercase",letterSpacing:"0.04em",margin:0}},"Digest & Server Status"),
              React.createElement("div",{style:{display:"flex",gap:8,alignItems:"center",flexWrap:"wrap"}},
                React.createElement("button",{onClick:()=>ops.run(),disabled:ops.running,
                  style:{display:"flex",alignItems:"center",gap:6,padding:"6px 14px",borderRadius:6,border:"none",background:ops.running?theme.surfaceActive:theme.accent,color:ops.running?theme.textMuted:"#fff",fontSize:12,fontWeight:600,fontFamily:mono,cursor:ops.running?"wait":"pointer"}},
                  ops.running ? React.createElement(Icons.Loader) : React.createElement(Icons.Play),
                  ops.digest ? "Re-run" : "Run")
              )
            ),
            ops.error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono,marginBottom:12}}, ops.error),
            !ops.digest && !ops.error && React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans,lineHeight:1.6}},
              "Runs remind_me_digest and remind_me_server_status. Sensitive memories are never included in a digest."
            ),
            ops.digest && React.createElement("div",{style:{display:"grid",gridTemplateColumns:"repeat(auto-fit, minmax(260px, 1fr))",gap:20}},
              React.createElement("div",null,
                React.createElement("div",{style:labelSt}, (ops.digest.recent_total||0)+" new in "+ops.digest.since_days+" days"),
                React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:6}},
                  (ops.digest.recent_memories||[]).map(m=>React.createElement("div",{key:m.id,style:{fontSize:13,fontFamily:sans,color:theme.text,padding:"8px 10px",background:theme.surfaceActive,borderRadius:5,lineHeight:1.5}},
                    React.createElement(CategoryBadge,{category:m.category}),
                    React.createElement("div",{style:{marginTop:4,wordBreak:"break-word"}}, m.content.length>160 ? m.content.slice(0,160)+"…" : m.content)
                  )),
                  (ops.digest.recent_memories||[]).length===0 && React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans}},"Nothing new in this window.")
                )
              ),
              React.createElement("div",null,
                React.createElement("div",{style:labelSt},"Reminders"),
                React.createElement("div",{style:{fontFamily:mono,fontSize:12,color:theme.textSecondary,lineHeight:1.9,marginBottom:16}},
                  React.createElement("div",null, (ops.digest.reminders_overdue||[]).length+" overdue · "+(ops.digest.reminders_upcoming||[]).length+" upcoming"),
                  (ops.digest.reminders_overdue||[]).map(r=>React.createElement("div",{key:"o"+r.id,style:{color:theme.danger,wordBreak:"break-word"}}, "• "+formatWhen(r.remind_at)+" — "+r.content)),
                  (ops.digest.reminders_upcoming||[]).map(r=>React.createElement("div",{key:"u"+r.id,style:{color:theme.warning,wordBreak:"break-word"}}, "• "+formatWhen(r.remind_at)+" — "+r.content))
                ),
                ops.status && React.createElement(React.Fragment,null,
                  React.createElement("div",{style:labelSt},"Server status"),
                  React.createElement("div",{style:{fontFamily:mono,fontSize:12,color:theme.textSecondary,lineHeight:1.9}},
                    React.createElement("div",null,"Version ",React.createElement("code",{style:{color:theme.text}}, ops.status.version)),
                    React.createElement("div",null,"Memories ",React.createElement("code",{style:{color:theme.text}}, ops.status.memory_count)),
                    React.createElement("div",{style:{color:ops.status.schema_current?theme.textSecondary:theme.danger}},
                      "Schema ",React.createElement("code",{style:{color:ops.status.schema_current?theme.text:theme.danger}}, ops.status.schema_version),
                      ops.status.schema_current ? " (current)" : " — this build expects "+ops.status.expected_schema_version),
                    React.createElement("div",null,"Backups ",React.createElement("code",{style:{color:theme.text}}, ops.status.backup_count),
                      ops.status.latest_backup ? " · latest "+formatWhen(ops.status.latest_backup.created_at) : ""),
                    React.createElement("div",null,"Scheduler ",React.createElement("code",{style:{color:ops.status.scheduler&&ops.status.scheduler.running?theme.success:theme.textMuted}}, ops.status.scheduler&&ops.status.scheduler.running?"running":"stopped")),
                    React.createElement("div",null,"Watcher ",React.createElement("code",{style:{color:ops.status.watcher&&ops.status.watcher.running?theme.success:theme.textMuted}}, ops.status.watcher&&ops.status.watcher.running?"running":(ops.status.watcher&&ops.status.watcher.enabled?"configured, not running":"off"))),
                    // A subsystem this build never had reads as
                    // "not implemented", not as "stopped" -- the server
                    // reports the distinction, so the panel keeps it.
                    ["mcp","dashboard","embeddings","sync"].map(k=>ops.status[k] && React.createElement("div",{key:k},
                      k.charAt(0).toUpperCase()+k.slice(1)+" ",
                      React.createElement("code",{style:{color:ops.status[k].state==="active"?theme.success:theme.textMuted},title:ops.status[k].reason||""}, ops.status[k].state==="active"?"active":"not implemented")
                    ))
                  )
                )
              )
            )
          )
        )
      )
    ),
    // Modals
    React.createElement(Modal,{open:showAddModal,onClose:()=>setShowAddModal(false),title:"Add Memory",width:520},
      React.createElement(MemoryForm,{onSubmit:handleAdd,onCancel:()=>setShowAddModal(false)})
    ),
    React.createElement(Modal,{open:showImportModal,onClose:()=>setShowImportModal(false),title:"Import Chat History",width:560},
      React.createElement(ImportForm,{onComplete:()=>store.refresh(),onCancel:()=>setShowImportModal(false)})
    ),
    React.createElement(Modal,{open:!!editMemory,onClose:()=>setEditMemory(null),title:"Edit Memory",width:520},
      editMemory && React.createElement(MemoryForm,{initial:editMemory,onSubmit:handleEdit,onCancel:()=>setEditMemory(null)})
    ),
    React.createElement(Modal,{open:!!wikiEdit,onClose:()=>setWikiEdit(null),title:wikiEdit&&wikiEdit.page?"Edit Wiki Page":"New Wiki Page",width:720},
      wikiEdit && React.createElement(WikiPageForm,{initial:wikiEdit.page,onSubmit:handleWikiWrite,onLoadSchema:wikiStore.readSchema,onCancel:()=>setWikiEdit(null)})
    ),
    React.createElement(Modal,{open:showCompile,onClose:()=>setShowCompile(false),title:"Compile the Wiki",width:720},
      showCompile && React.createElement(WikiCompilePanel,{onCompile:wikiStore.compile,onClose:()=>setShowCompile(false)})
    ),
    React.createElement(Modal,{open:!!wikiDeleteConfirm,onClose:()=>setWikiDeleteConfirm(null),title:"Delete Wiki Page",width:420},
      wikiDeleteConfirm && React.createElement(React.Fragment,null,
        React.createElement("p",{style:{color:theme.textSecondary,fontFamily:sans,fontSize:14,lineHeight:1.6}},
          "Permanently delete ", React.createElement("b",{style:{color:theme.text}}, wikiDeleteConfirm.title),
          "? The markdown file is removed from disk and the index regenerated. Memories are untouched, but [[links]] to this page will dangle. This cannot be undone."),
        React.createElement("div",{style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:20}},
          React.createElement("button",{onClick:()=>setWikiDeleteConfirm(null),style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
          React.createElement("button",{onClick:()=>handleWikiDelete(wikiDeleteConfirm.slug),style:{padding:"8px 20px",borderRadius:6,border:"none",background:theme.danger,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}},"Delete")
        )
      )
    ),
    React.createElement(Modal,{open:!!remindMemory,onClose:()=>setRemindMemory(null),title:remindMemory&&remindMemory.remind_at?"Change Reminder":"Set Reminder",width:480},
      remindMemory && React.createElement(ReminderForm,{memory:remindMemory,onSubmit:handleSetReminder,onCancel:()=>setRemindMemory(null)})
    ),
    React.createElement(Modal,{open:!!showSaveSearch,onClose:()=>setShowSaveSearch(null),title:"Save Search",width:520},
      showSaveSearch && React.createElement(SaveSearchForm,{initial:showSaveSearch,onSubmit:handleSaveSearch,onCancel:()=>setShowSaveSearch(null)})
    ),
    React.createElement(Modal,{open:!!deleteSearchConfirm,onClose:()=>setDeleteSearchConfirm(null),title:"Delete Saved Search",width:400},
      React.createElement("p",{style:{color:theme.textSecondary,fontFamily:sans,fontSize:14,lineHeight:1.6}}, "Delete the saved search ", React.createElement("code",{style:{fontFamily:mono,color:theme.text}},deleteSearchConfirm), ", along with the seen-memory rows its watch tracking accumulated? The memories it matches are not touched."),
      React.createElement("div",{style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:20}},
        React.createElement("button",{onClick:()=>setDeleteSearchConfirm(null),style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
        React.createElement("button",{onClick:()=>handleDeleteSavedSearch(deleteSearchConfirm),style:{padding:"8px 20px",borderRadius:6,border:"none",background:theme.danger,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}},"Delete")
      )
    ),
    React.createElement(Modal,{open:!!deleteConfirm,onClose:()=>setDeleteConfirm(null),title:"Delete Memory",width:400},
      React.createElement("p",{style:{color:theme.textSecondary,fontFamily:sans,fontSize:14,lineHeight:1.6}}, "Are you sure you want to permanently delete memory ", React.createElement("code",{style:{fontFamily:mono,color:theme.text}},deleteConfirm), "? This cannot be undone."),
      React.createElement("div",{style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:20}},
        React.createElement("button",{onClick:()=>setDeleteConfirm(null),style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
        React.createElement("button",{onClick:()=>handleDelete(deleteConfirm),style:{padding:"8px 20px",borderRadius:6,border:"none",background:theme.danger,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}},"Delete")
      )
    )
  );
}

ReactDOM.createRoot(document.getElementById("root")).render(React.createElement(App));
