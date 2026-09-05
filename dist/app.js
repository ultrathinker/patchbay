/* Patchbay popover UI (S13, WINDOW_UI_PLAN.md).
 *
 * Preact + htm, both vendored next to this file (see vendor/PROVENANCE.txt).
 * No npm, no build step, no JSX — htm compiles tagged template literals at
 * runtime without dynamic code evaluation, so the strict CSP stays exactly as
 * it was.
 *
 * ## The three rules that shape this file
 *
 * 1. **Never render untrusted text as markup.** Agent names, versions and
 *    upstream error strings arrive from processes that authenticate nothing.
 *    Preact sets text as text, and no raw-HTML sink is used anywhere here —
 *    `frontend_never_uses_raw_html_sinks` in `src/ui.rs` fails the build if one
 *    appears (WINDOW_UI_PLAN W-D11 layer 2). That test is a plain substring
 *    scan with no comment handling, deliberately: a dumber check is a more
 *    trustworthy one, so the forbidden names are not written out even in prose.
 *
 * 2. **A pending row is immune to inbound snapshots.** The backend pushes a
 *    fresh snapshot whenever anything changes, including while a toggle the
 *    user just clicked is still in flight. Without the in-flight map below, an
 *    unrelated event would snap that switch back mid-click.
 *
 * 3. **A visible list never reorders itself.** Order is captured when a screen
 *    is entered and held until it is left; an agent that connects meanwhile
 *    arrives as a banner the user can accept, not as a row that jumps into
 *    place under their cursor (§4.2).
 */

(function () {
  "use strict";

  var h = preact.h;
  var render = preact.render;
  var useState = preactHooks.useState;
  var useEffect = preactHooks.useEffect;
  var useRef = preactHooks.useRef;
  var useMemo = preactHooks.useMemo;
  var html = htm.bind(h);

  var invoke = window.__TAURI__.core.invoke;
  var listen = window.__TAURI__.event.listen;

  /** Below this, a progress hint is visual noise rather than information. */
  var PENDING_AFTER_MS = 150;
  /** An identity unseen for this long is "unused" — the junk-cleanup bucket. */
  var UNUSED_AFTER_DAYS = 30;

  // -------------------------------------------------------------------------
  // helpers
  // -------------------------------------------------------------------------

  function cx() {
    var out = [];
    for (var i = 0; i < arguments.length; i++) {
      if (arguments[i]) out.push(arguments[i]);
    }
    return out.join(" ");
  }

  function plural(n, one, many) {
    return n + " " + (n === 1 ? one : many);
  }

  function daysSince(rfc3339) {
    if (!rfc3339) return null;
    var t = Date.parse(rfc3339);
    if (isNaN(t)) return null;
    return (Date.now() - t) / 86400000;
  }

  /** Coarse, human relative time. Precision here would be false precision. */
  function ago(seconds) {
    if (seconds < 60) return "just now";
    var m = Math.floor(seconds / 60);
    if (m < 60) return plural(m, "minute", "minutes") + " ago";
    var hrs = Math.floor(m / 60);
    if (hrs < 24) return plural(hrs, "hour", "hours") + " ago";
    var d = Math.floor(hrs / 24);
    return plural(d, "day", "days") + " ago";
  }

  function agoFromDate(rfc3339) {
    var days = daysSince(rfc3339);
    if (days === null) return null;
    return ago(days * 86400);
  }

  function shortDate(rfc3339) {
    if (!rfc3339) return null;
    var t = Date.parse(rfc3339);
    if (isNaN(t)) return null;
    var d = new Date(t);
    return d.toLocaleDateString(undefined, {
      day: "numeric",
      month: "short",
      year: "numeric",
    });
  }

  /** An agent nobody has used since first contact, or not in a long while. */
  function isUnused(agent) {
    if (agent.connected) return false;
    if (!agent.last_seen) return true;
    var days = daysSince(agent.last_seen);
    return days !== null && days >= UNUSED_AFTER_DAYS;
  }

  function agentMeta(agent) {
    if (agent.connected) {
      return "connected · active " + ago(agent.idle_secs || 0);
    }
    if (agent.denied && !agent.first_seen) {
      // A denial writes only to forbidden_clients, never to seen_clients, so
      // this identity genuinely has no history to show.
      return "denied at gateway · no session history";
    }
    if (agent.last_seen) return "last seen " + agoFromDate(agent.last_seen);
    if (agent.first_seen) {
      return "first seen " + shortDate(agent.first_seen) + " · never since";
    }
    return "no session history";
  }

  function jackMeta(jack) {
    var t = jack.transport === "stdio" ? "local" : "http";
    if (jack.error) return t + " · " + jack.error;
    if (!jack.patched) return t + " · off";
    if (jack.status === "starting") return t + " · starting…";
    if (jack.tool_count === null || jack.tool_count === undefined) {
      return t + " · " + jack.status;
    }
    return t + " · " + plural(jack.tool_count, "tool", "tools");
  }

  // -------------------------------------------------------------------------
  // small components
  // -------------------------------------------------------------------------

  function Switch(props) {
    return html`
      <span
        class=${cx("switch", props.variant && "switch--" + props.variant)}
        role="switch"
        aria-checked=${props.on ? "true" : "false"}
        aria-label=${props.label}
      ></span>
    `;
  }

  function Header(props) {
    var g = props.snapshot.gateway;
    var jacks = props.snapshot.jacks;
    var patched = jacks.filter(function (j) {
      return j.patched;
    }).length;

    var subtitle =
      g.status === "failed"
        ? "Gateway failed: " + (g.error || "unknown reason")
        : patched +
          " of " +
          jacks.length +
          " patched · " +
          plural(g.session_count, "agent", "agents") +
          " connected";

    return html`
      <div class="header" data-tauri-drag-region="deep">
        <div class="header-top">
          <span class=${cx("dot", g.status)} aria-hidden="true"></span>
          <span class="title">Patchbay</span>
          <button
            class="url"
            title="Copy the gateway URL"
            onClick=${function () {
              invoke("ui_copy_url");
              props.onToast("Gateway URL copied");
            }}
          >
            ${"127.0.0.1:" + g.port}
          </button>
          <button
            class="close"
            title="Close (keeps running in the tray)"
            aria-label="Close"
            onClick=${function () { invoke("ui_close_window"); }}
          >
            ✕
          </button>
        </div>
        <div class="subtitle">${subtitle}</div>
        ${g.config_error &&
        html`
          <div class="banner">
            <span aria-hidden="true">⚠</span>
            <span class="banner-text"
              >Config file problem: ${g.config_error}</span
            >
            <button class="btn" onClick=${function () { invoke("ui_open_config"); }}>
              Open
            </button>
          </div>
        `}
        ${g.status === "failed" &&
        html`
          <div class="banner">
            <span aria-hidden="true">⚠</span>
            <span class="banner-text"
              >Port ${g.port} could not be bound. No agent can reach Patchbay
              until this is fixed.</span
            >
            <button class="btn" onClick=${function () { invoke("ui_retry_gateway"); }}>
              Retry
            </button>
          </div>
        `}
      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // jacks
  // -------------------------------------------------------------------------

  function JackRow(props) {
    var jack = props.jack;
    return html`
      <button
        class=${cx("row", props.pending && "row--pending")}
        onClick=${props.onToggle}
        aria-disabled=${props.disabled ? "true" : "false"}
      >
        <${Switch} on=${jack.patched} label=${jack.name} />
        <span class="row-body">
          <span class="row-name">${jack.name}</span>
          <span class=${cx("row-meta", jack.error && "err")}>${jackMeta(jack)}</span>
        </span>
        ${jack.sensitive && html`<span class="chip" title="Sensitive server">prod</span>`}
        ${jack.error && html`<span class="badge" aria-hidden="true">⚠</span>`}
        <span
          class="rowaction"
          role="button"
          tabindex="0"
          title=${"Remove " + jack.name}
          aria-label=${"Remove " + jack.name}
          onClick=${function (e) {
            // The row itself toggles; this must not do both.
            e.stopPropagation();
            props.onRemove();
          }}
        >
          ✕
        </span>
      </button>
    `;
  }

  function JacksScreen(props) {
    var snapshot = props.snapshot;
    var q = props.query.trim().toLowerCase();
    var jacks = useMemo(
      function () {
        if (!q) return snapshot.jacks;
        return snapshot.jacks.filter(function (j) {
          return j.name.toLowerCase().indexOf(q) !== -1;
        });
      },
      [snapshot.jacks, q]
    );

    if (snapshot.jacks.length === 0) {
      return html`
        <div class="empty">
          <p>No servers yet. Add one to expose its tools to every agent.</p>
          <button class="btn primary" onClick=${props.onAdd}>Add server</button>
        </div>
      `;
    }

    var gatewayDown = snapshot.gateway.status === "failed";

    return html`
      <div class="list">
        ${jacks.length === 0 &&
        html`<div class="empty"><p>No server matches “${props.query}”.</p></div>`}
        ${jacks.map(function (jack) {
          if (props.confirming === "on:" + jack.name) {
            return html`
              <div class="confirm" key=${"c-" + jack.name}>
                <span class="confirm-text"
                  >Turn on ${jack.name}? Every agent gets live access.</span
                >
                <button class="btn" onClick=${props.onCancelConfirm}>Cancel</button>
                <button class="btn primary" onClick=${function () { props.onConfirm(jack); }}>
                  Turn on
                </button>
              </div>
            `;
          }
          if (props.confirming === "rm:" + jack.name) {
            return html`
              <div class="confirm" key=${"r-" + jack.name}>
                <span class="confirm-text">Remove ${jack.name}?</span>
                <button class="btn" onClick=${props.onCancelConfirm}>Cancel</button>
                <button class="btn primary" onClick=${function () { props.onRemove(jack); }}>
                  Remove
                </button>
              </div>
            `;
          }
          return html`
            <${JackRow}
              key=${jack.name}
              jack=${jack}
              pending=${props.pending[jack.name] === true}
              disabled=${gatewayDown}
              onToggle=${function () { props.onToggle(jack); }}
              onRemove=${function () { props.onAskRemove(jack); }}
            />
          `;
        })}
      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // agents
  // -------------------------------------------------------------------------

  var FILTERS = [
    { key: "all", label: "All" },
    { key: "connected", label: "Connected" },
    { key: "custom", label: "Custom" },
    { key: "denied", label: "Denied" },
    { key: "unused", label: "Unused" },
  ];

  function matchesFilter(agent, key) {
    if (key === "all") return true;
    if (key === "connected") return agent.connected;
    if (key === "custom") return agent.custom_state !== "none";
    if (key === "denied") return agent.denied;
    if (key === "unused") return isUnused(agent);
    return true;
  }

  function AgentRow(props) {
    var a = props.agent;
    return html`
      <button class="row" onClick=${props.onClick}>
        ${props.selectMode
          ? html`<${Switch} on=${props.selected} label=${"Select " + a.name} />`
          : html`<span
              class=${cx("dot", a.connected ? "running" : a.denied ? "failed" : "")}
              aria-hidden="true"
            ></span>`}
        <span class="row-body">
          <span class="row-name">${a.name}</span>
          <span class="row-meta">${agentMeta(a)}</span>
        </span>
        ${a.custom_state === "enabled" && html`<span class="chip">Custom</span>`}
        ${a.custom_state === "preserved_disabled" &&
        html`<span class="chip" title="Custom is off, but its list is kept">Kept</span>`}
        ${a.denied && html`<span class="chip">Denied</span>`}
        ${!props.selectMode && html`<span class="badge" aria-hidden="true">›</span>`}
      </button>
    `;
  }

  function AgentsScreen(props) {
    var snapshot = props.snapshot;
    var q = props.query.trim().toLowerCase();

    var visible = useMemo(
      function () {
        var order = props.order;
        var known = snapshot.agents.filter(function (a) {
          return order.indexOf(a.name) !== -1;
        });
        known.sort(function (x, y) {
          return order.indexOf(x.name) - order.indexOf(y.name);
        });
        return known.filter(function (a) {
          if (!matchesFilter(a, props.filter)) return false;
          if (!q) return true;
          return a.name.toLowerCase().indexOf(q) !== -1;
        });
      },
      [snapshot.agents, props.order, props.filter, q]
    );

    var arrivals = snapshot.agents.filter(function (a) {
      return props.order.indexOf(a.name) === -1;
    });

    if (snapshot.agents.length === 0) {
      return html`
        <div class="empty">
          <p>
            No agent has connected yet. Point one at
            http://127.0.0.1:${snapshot.gateway.port}/mcp
          </p>
          <button class="btn primary" onClick=${function () { invoke("ui_copy_url"); }}>
            Copy URL
          </button>
        </div>
      `;
    }

    return html`
      <div class="list">
        ${arrivals.length > 0 &&
        html`
          <button class="row" onClick=${props.onAcceptArrivals}>
            <span class="row-body">
              <span class="row-name"
                >${plural(arrivals.length, "new agent", "new agents")}</span
              >
              <span class="row-meta">Show in the list</span>
            </span>
            <span class="badge" aria-hidden="true">＋</span>
          </button>
        `}
        ${visible.length === 0 &&
        html`<div class="empty"><p>No agent matches this filter.</p></div>`}
        ${visible.map(function (a) {
          return html`
            <${AgentRow}
              key=${a.name}
              agent=${a}
              selectMode=${props.selectMode}
              selected=${props.selected.indexOf(a.name) !== -1}
              onClick=${function () { props.onRowClick(a); }}
            />
          `;
        })}
      </div>
    `;
  }

  function AgentDetail(props) {
    var a = props.agent;
    var custom = a.custom_state === "enabled";
    var preserved = a.custom_state === "preserved_disabled";

    return html`
      <div class="list">
        <div class="detail-head">
          <div class="row-name">${a.name}</div>
          <div class="row-meta">
            ${agentMeta(a)}${a.version ? " · v" + a.version : ""}
          </div>
          ${a.first_seen &&
          html`<div class="row-meta">first seen ${shortDate(a.first_seen)}</div>`}
        </div>

        <button
          class="row row--master"
          onClick=${function () {
            props.onSetCustom(a, !custom);
          }}
        >
          <${Switch} on=${custom} variant="master" label="Custom permissions" />
          <span class="row-body">
            <span class="row-name">Custom permissions</span>
            <span class="row-meta">
              ${custom
                ? "This agent uses its own on/off list."
                : preserved
                ? "Off — its previous list is kept and will be restored."
                : "Off — this agent follows the global list."}
            </span>
          </span>
          <span
            class="rowaction rowaction--danger"
            role="button"
            tabindex="0"
            title=${"Delete " + a.name}
            aria-label=${"Delete agent " + a.name}
            onClick=${function (e) {
              // The row toggles Custom; this must not also do that.
              e.stopPropagation();
              props.onDelete(a);
            }}
          >
            ✕
          </span>
        </button>

        <div class="subhead">
          ${custom ? "This agent's own list" : "Following the global list"}
        </div>

        ${a.overrides.map(function (o) {
          return html`
            <button
              class="row"
              key=${o.jack}
              aria-disabled=${custom ? "false" : "true"}
              onClick=${function () {
                if (custom) props.onSetOverride(a, o.jack, !o.on);
              }}
            >
              <${Switch} on=${o.on} label=${o.jack} />
              <span class="row-body"><span class="row-name">${o.jack}</span></span>
            </button>
          `;
        })}

        ${preserved &&
        html`
          <button class="btn" onClick=${function () { props.onResetCustom(a); }}>
            Reset to global
          </button>
        `}

        <button
          class="row"
          onClick=${function () {
            props.onSetDenied(a, !a.denied);
          }}
        >
          <${Switch} on=${a.denied} label="Denied" />
          <span class="row-body">
            <span class="row-name">Denied</span>
            <span class="row-meta">Refuse this identity at the gateway.</span>
          </span>
        </button>

      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // add jack
  // -------------------------------------------------------------------------

  /** Mirrors `config::is_valid_jack_name`, so the error appears under the
   *  field instead of coming back as a failed command. */
  function jackNameError(name, existing) {
    if (!name) return "A name is required.";
    if (name.length > 40) return "At most 40 characters.";
    if (name.indexOf("__") !== -1) return "Cannot contain “__”.";
    if (!/^[A-Za-z0-9_-]+$/.test(name)) return "Letters, digits, - and _ only.";
    if (existing.indexOf(name) !== -1) return "A server with this name already exists.";
    return null;
  }

  function AddJackScreen(props) {
    var st = useState({
      name: "",
      transport: "streamable-http",
      url: "",
      command: "",
      args: "",
      pairKey: "",
      pairValue: "",
      patched: false,
    });
    var f = st[0];
    var setF = st[1];
    var errState = useState(null);
    var submitError = errState[0];
    var setSubmitError = errState[1];

    function set(key, value) {
      var next = Object.assign({}, f);
      next[key] = value;
      setF(next);
    }

    var existing = props.snapshot.jacks.map(function (j) {
      return j.name;
    });
    var nameErr = f.name ? jackNameError(f.name, existing) : null;
    var http = f.transport === "streamable-http";
    var ready =
      !jackNameError(f.name, existing) && (http ? !!f.url.trim() : !!f.command.trim());

    function submit() {
      var pairs = {};
      if (f.pairKey.trim()) pairs[f.pairKey.trim()] = f.pairValue;
      var spec = http
        ? {
            name: f.name,
            patched: f.patched,
            transport: "streamable-http",
            url: f.url.trim(),
            headers: pairs,
            sharing: "shared",
          }
        : {
            name: f.name,
            patched: f.patched,
            transport: "stdio",
            command: f.command.trim(),
            args: f.args.trim() ? f.args.trim().split(/\s+/) : [],
            env: pairs,
            sharing: "shared",
          };
      invoke("ui_add_jack", { spec: spec })
        .then(function () {
          props.onDone();
        })
        .catch(function (e) {
          setSubmitError(String(e));
        });
    }

    return html`
      <div class="list form">
        <label class="field">
          <span class="field-label">Name</span>
          <input
            class="input"
            value=${f.name}
            placeholder="github"
            onInput=${function (e) { set("name", e.target.value); }}
          />
          <span class=${cx("field-hint", nameErr && "err")}>
            ${nameErr || "Letters, digits, - and _"}
          </span>
        </label>

        <div class="field">
          <span class="field-label">Transport</span>
          <div class="segmented">
            <button
              class=${cx("btn", http && "primary")}
              onClick=${function () { set("transport", "streamable-http"); }}
            >
              HTTP URL
            </button>
            <button
              class=${cx("btn", !http && "primary")}
              onClick=${function () { set("transport", "stdio"); }}
            >
              Local command
            </button>
          </div>
        </div>

        ${http
          ? html`
              <label class="field">
                <span class="field-label">URL</span>
                <input
                  class="input"
                  value=${f.url}
                  placeholder="https://host/mcp"
                  onInput=${function (e) { set("url", e.target.value); }}
                />
              </label>
            `
          : html`
              <label class="field">
                <span class="field-label">Command</span>
                <input
                  class="input"
                  value=${f.command}
                  placeholder="npx"
                  onInput=${function (e) { set("command", e.target.value); }}
                />
              </label>
              <label class="field">
                <span class="field-label">Arguments</span>
                <input
                  class="input"
                  value=${f.args}
                  placeholder="-y some-mcp"
                  onInput=${function (e) { set("args", e.target.value); }}
                />
              </label>
            `}

        <div class="field">
          <span class="field-label">${http ? "Header" : "Environment variable"}</span>
          <div class="pair">
            <input
              class="input"
              value=${f.pairKey}
              placeholder=${http ? "Authorization" : "DB_TOKEN"}
              onInput=${function (e) { set("pairKey", e.target.value); }}
            />
            <input
              class="input"
              type="password"
              value=${f.pairValue}
              placeholder="value"
              onInput=${function (e) { set("pairValue", e.target.value); }}
            />
          </div>
          <span class="field-hint">
            Stored encrypted with your Windows account. Never shown again.
          </span>
        </div>

        <button class="row" onClick=${function () { set("patched", !f.patched); }}>
          <${Switch} on=${f.patched} label="Patch immediately" />
          <span class="row-body">
            <span class="row-name">Patch immediately</span>
            <span class="row-meta">Off by default — opt in deliberately.</span>
          </span>
        </button>

        ${submitError &&
        html`<div class="banner"><span class="banner-text">${submitError}</span></div>`}

        <div class="form-actions">
          <button class="btn" onClick=${props.onDone}>Cancel</button>
          <button class="btn primary" disabled=${!ready} onClick=${submit}>Add</button>
        </div>
      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // settings
  // -------------------------------------------------------------------------

  function SettingsScreen(props) {
    var s = props.snapshot.settings;
    var g = props.snapshot.gateway;
    var portState = useState(String(s.port));
    var port = portState[0];
    var setPort = portState[1];

    function toggle(cmd, value) {
      invoke(cmd, { on: value }).catch(function (e) {
        props.onToast(String(e));
      });
    }

    return html`
      <div class="list form">
        <div class="field">
          <span class="field-label">Interface</span>
          ${[
            { key: "tray", label: "Tray menu only" },
            { key: "window", label: "Window" },
            { key: "both", label: "Both — window on left, menu on right" },
          ].map(function (opt) {
            return html`
              <button
                class="row"
                key=${opt.key}
                onClick=${function () {
                  invoke("ui_set_ui_mode", { mode: opt.key }).catch(function (e) {
                    props.onToast(String(e));
                  });
                }}
              >
                <span
                  class=${cx("radio", s.ui_mode === opt.key && "on")}
                  role="radio"
                  aria-checked=${s.ui_mode === opt.key ? "true" : "false"}
                ></span>
                <span class="row-body"><span class="row-name">${opt.label}</span></span>
              </button>
            `;
          })}
        </div>

        <div class="field">
          <span class="field-label">Gateway</span>
          <div class="pair">
            <input
              class="input"
              value=${port}
              onInput=${function (e) { setPort(e.target.value); }}
            />
            <button
              class="btn"
              onClick=${function () {
                var n = parseInt(port, 10);
                if (!n || n < 1 || n > 65535) {
                  props.onToast("Port must be between 1 and 65535.");
                  return;
                }
                invoke("ui_set_port", { port: n }).catch(function (e) {
                  props.onToast(String(e));
                });
              }}
            >
              Apply
            </button>
          </div>
          <span class="field-hint">
            ${g.status === "running"
              ? "Running · " + plural(g.session_count, "session", "sessions")
              : g.status}
            — changing the port drops every agent session.
          </span>
        </div>

        <button class="row" onClick=${function () { toggle("ui_set_autostart", !s.autostart); }}>
          <${Switch} on=${s.autostart} label="Start with Windows" />
          <span class="row-body"><span class="row-name">Start with Windows</span></span>
        </button>

        <button
          class="row"
          onClick=${function () { toggle("ui_set_require_approval", !s.require_approval); }}
        >
          <${Switch} on=${s.require_approval} label="Require approval for new agents" />
          <span class="row-body">
            <span class="row-name">Require approval for new agents</span>
            <span class="row-meta">Ask once when an unknown identity connects.</span>
          </span>
        </button>

        <button
          class="row"
          onClick=${function () { toggle("ui_set_request_logging", !s.request_logging); }}
        >
          <${Switch} on=${s.request_logging} label="Request logging" />
          <span class="row-body">
            <span class="row-name">Request logging</span>
            <span class="row-meta">Every MCP request, secrets redacted. Off by default.</span>
          </span>
        </button>

        <div class="form-actions">
          <button class="btn" onClick=${function () { invoke("ui_open_config"); }}>
            Open config file
          </button>
          <button class="btn" onClick=${function () { invoke("ui_open_logs"); }}>
            Open logs folder
          </button>
        </div>

        <div class="field-hint">
          Patchbay ${s.version} ·
          <button class="link" onClick=${function () { invoke("ui_about"); }}>About</button>
        </div>
      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // footer
  // -------------------------------------------------------------------------

  function Footer(props) {
    function tab(key, label) {
      return html`
        <button
          class="link"
          aria-current=${props.screen === key ? "true" : "false"}
          onClick=${function () { props.onScreen(key); }}
        >
          ${label}
        </button>
      `;
    }
    return html`
      <div class="footer">
        ${tab("jacks", "Jacks")} ${tab("agents", "Agents " + props.agentCount)}
        ${tab("settings", "Settings")}
        <span class="spacer"></span>
        <button class="link" title="Open the logs folder" onClick=${function () { invoke("ui_open_logs"); }}>
          Logs
        </button>
        <button class="link" title="Re-read patchbay.json" onClick=${function () { invoke("ui_reload_config"); }}>
          Reload
        </button>
        <button class="link" title="Quit Patchbay" onClick=${function () { invoke("ui_quit"); }}>
          Quit
        </button>
      </div>
    `;
  }

  // -------------------------------------------------------------------------
  // app
  // -------------------------------------------------------------------------

  function App() {
    var s0 = useState(null);
    var snapshot = s0[0];
    var setSnapshot = s0[1];

    var s1 = useState("jacks");
    var screen = s1[0];
    var setScreen = s1[1];

    var s2 = useState("");
    var query = s2[0];
    var setQuery = s2[1];

    var s3 = useState({});
    var pending = s3[0];
    var setPending = s3[1];

    var s4 = useState(null);
    var confirming = s4[0];
    var setConfirming = s4[1];

    var s5 = useState(null);
    var toast = s5[0];
    var setToast = s5[1];

    var s6 = useState("all");
    var filter = s6[0];
    var setFilter = s6[1];

    var s7 = useState(false);
    var selectMode = s7[0];
    var setSelectMode = s7[1];

    var s8 = useState([]);
    var selected = s8[0];
    var setSelected = s8[1];

    var s9 = useState(null);
    var detail = s9[0];
    var setDetail = s9[1];

    var s10 = useState(null);
    var undo = s10[0];
    var setUndo = s10[1];

    var s11 = useState([]);
    var order = s11[0];
    var setOrder = s11[1];

    var inFlight = useRef({});
    var searchRef = useRef(null);

    // ---- live state ------------------------------------------------------
    useEffect(function () {
      var unlisten = null;
      invoke("ui_snapshot").then(setSnapshot);
      listen("patchbay://state", function (event) {
        var incoming = event.payload;
        setSnapshot(function (prev) {
          if (!prev) return incoming;
          var held = inFlight.current;
          if (Object.keys(held).length === 0) return incoming;
          var merged = Object.assign({}, incoming);
          merged.jacks = incoming.jacks.map(function (j) {
            if (!held[j.name]) return j;
            var previous = prev.jacks.filter(function (p) {
              return p.name === j.name;
            })[0];
            return previous ? Object.assign({}, j, { patched: previous.patched }) : j;
          });
          return merged;
        });
      }).then(function (fn) {
        unlisten = fn;
      });
      return function () {
        if (unlisten) unlisten();
      };
    }, []);

    // Capture the agent order when the screen is entered, and hold it (rule 3).
    useEffect(
      function () {
        if (screen !== "agents" || !snapshot) return;
        // Capture on ENTRY, and also the first time a snapshot arrives while
        // already here — reaching this screen before the first snapshot landed
        // (a slow start, or a click during load) used to leave `order` empty
        // forever, which renders as "no agent matches this filter" with every
        // agent piled into the new-arrivals banner.
        if (order.length === 0) {
          setOrder(sortedAgentNames(snapshot.agents));
        }
      },
      [screen, snapshot]
    );

    // Leaving the Agents screen releases the held order, so the next visit sorts
    // fresh rather than pinning last week's arrangement.
    useEffect(
      function () {
        if (screen !== "agents" && order.length > 0) setOrder([]);
      },
      [screen]
    );

    function sortedAgentNames(agents) {
      var copy = agents.slice();
      copy.sort(function (x, y) {
        if (x.connected !== y.connected) return x.connected ? -1 : 1;
        var xs = x.last_seen ? Date.parse(x.last_seen) : 0;
        var ys = y.last_seen ? Date.parse(y.last_seen) : 0;
        return ys - xs;
      });
      return copy.map(function (a) {
        return a.name;
      });
    }

    // ---- keyboard (§4.3) -------------------------------------------------
    useEffect(function () {
      function onKey(e) {
        var typing =
          e.target && (e.target.tagName === "INPUT" || e.target.tagName === "TEXTAREA");

        if (e.key === "Escape") {
          // While typing, Escape belongs to the field: it clears the filter and
          // gets out of the way. It must not close the window from under a
          // half-typed server name.
          if (typing) {
            setQuery("");
            if (e.target.blur) e.target.blur();
            return;
          }
          if (confirming) setConfirming(null);
          else if (selectMode) {
            setSelectMode(false);
            setSelected([]);
          } else if (detail) setDetail(null);
          else if (screen !== "jacks") setScreen("jacks");
          else if (query) setQuery("");
          else invoke("ui_close_window");
          return;
        }
        if (typing) return;

        function go(target) {
          setScreen(target);
          // Switching tabs must not leave the user parked in a child screen
          // they can no longer see the way back from.
          setDetail(null);
          setSelectMode(false);
          setSelected([]);
        }

        if (e.key === "/") {
          e.preventDefault();
          if (searchRef.current) searchRef.current.focus();
        } else if (e.key === "1") go("jacks");
        else if (e.key === "2") go("agents");
        else if (e.key === "3") go("settings");
      }
      window.addEventListener("keydown", onKey);
      return function () {
        window.removeEventListener("keydown", onKey);
      };
    }, [confirming, query, screen, selectMode, detail]);

    // ---- jack toggling ---------------------------------------------------
    function applyToggle(jack) {
      var name = jack.name;
      var want = !jack.patched;

      inFlight.current[name] = true;
      setSnapshot(function (prev) {
        if (!prev) return prev;
        var next = Object.assign({}, prev);
        next.jacks = prev.jacks.map(function (j) {
          return j.name === name ? Object.assign({}, j, { patched: want }) : j;
        });
        return next;
      });

      var slow = setTimeout(function () {
        setPending(function (p) {
          var n = Object.assign({}, p);
          n[name] = true;
          return n;
        });
      }, PENDING_AFTER_MS);

      // Everything that must happen however the call ends — success, refusal,
      // rejection or never answering at all. Kept in one place because the
      // first version cleared these only on the happy path, which left a failed
      // toggle pulsing forever and showing the state the user asked for rather
      // than the one they got.
      function settle() {
        clearTimeout(slow);
        clearTimeout(guard);
        delete inFlight.current[name];
        setPending(function (p) {
          var n = Object.assign({}, p);
          delete n[name];
          return n;
        });
      }

      // A command that never resolves (a child process wedged in its handshake)
      // would otherwise keep this row immune to every future snapshot, for the
      // life of the window. After 15 s, give up on being clever and let the
      // authoritative state through.
      var guard = setTimeout(function () {
        if (!inFlight.current[name]) return;
        settle();
        setToast("Toggling " + name + " is taking too long — showing the last known state.");
      }, 15000);

      invoke("ui_toggle_jack", { name: name, on: want })
        .then(function (result) {
          settle();
          if (!result) return;
          setSnapshot(function (prev) {
            if (!prev) return prev;
            var next = Object.assign({}, prev);
            next.jacks = prev.jacks.map(function (j) {
              if (j.name !== name) return j;
              var failed = result.status && result.status.indexOf("failed") === 0;
              return Object.assign({}, j, {
                patched: result.patched,
                // Clear a stale failure when the server comes up cleanly,
                // instead of leaving yesterday's error under a working row.
                error: failed ? result.status : null,
              });
            });
            return next;
          });
        })
        .catch(function (err) {
          settle();
          // Roll the switch back to where it was: the optimistic move was a
          // promise about what would happen, and it did not.
          setSnapshot(function (prev) {
            if (!prev) return prev;
            var next = Object.assign({}, prev);
            next.jacks = prev.jacks.map(function (j) {
              return j.name === name
                ? Object.assign({}, j, { patched: !want })
                : j;
            });
            return next;
          });
          setToast(String(err));
        });
    }

    function onToggleJack(jack) {
      if (jack.sensitive && !jack.patched) {
        setConfirming("on:" + jack.name);
        return;
      }
      applyToggle(jack);
    }

    // ---- agent actions ---------------------------------------------------
    function call(cmd, args) {
      return invoke(cmd, args).catch(function (e) {
        setToast(String(e));
      });
    }

    function deleteAgents(names) {
      call("ui_delete_agents", { names: names }).then(function (token) {
        if (token === undefined) return;
        setUndo({ token: token, count: names.length });
        setSelectMode(false);
        setSelected([]);
        setDetail(null);
      });
    }

    if (!snapshot) {
      return html`<div class="app"><div class="empty">Loading…</div></div>`;
    }

    var detailAgent = detail
      ? snapshot.agents.filter(function (a) {
          return a.name === detail;
        })[0]
      : null;

    return html`
      <div class="app">
        <${Header} snapshot=${snapshot} onToast=${setToast} />

        ${screen === "jacks" &&
        html`
          <div class="section">
            <h2>Servers</h2>
            <input
              class="search"
              type="search"
              placeholder="Filter…"
              value=${query}
              ref=${searchRef}
              onInput=${function (e) { setQuery(e.target.value); }}
            />
            <button class="btn icon" title="Add a server" onClick=${function () { setScreen("add"); }}>
              ＋
            </button>
          </div>
          <${JacksScreen}
            snapshot=${snapshot}
            query=${query}
            pending=${pending}
            confirming=${confirming}
            onToggle=${onToggleJack}
            onConfirm=${function (jack) { setConfirming(null); applyToggle(jack); }}
            onRemove=${function (jack) {
              setConfirming(null);
              call("ui_remove_jack", { name: jack.name });
            }}
            onAskRemove=${function (jack) { setConfirming("rm:" + jack.name); }}
            onCancelConfirm=${function () { setConfirming(null); }}
            onAdd=${function () { setScreen("add"); }}
          />
        `}

        ${screen === "add" &&
        html`
          <div class="section">
            <button class="link" onClick=${function () { setScreen("jacks"); }}>‹ Cancel</button>
            <h2>Add server</h2>
          </div>
          <${AddJackScreen}
            snapshot=${snapshot}
            onDone=${function () { setScreen("jacks"); }}
          />
        `}

        ${screen === "agents" && !detailAgent &&
        html`
          <div class="section">
            <h2>Agents</h2>
            <input
              class="search"
              type="search"
              placeholder="Filter…"
              value=${query}
              ref=${searchRef}
              onInput=${function (e) { setQuery(e.target.value); }}
            />
            <button
              class="btn icon"
              onClick=${function () {
                setSelectMode(!selectMode);
                setSelected([]);
              }}
            >
              ${selectMode ? "Done" : "Select"}
            </button>
          </div>
          <div class="chips">
            ${FILTERS.map(function (f) {
              return html`
                <button
                  class=${cx("chip", "chip-btn", filter === f.key && "on")}
                  key=${f.key}
                  onClick=${function () { setFilter(f.key); }}
                >
                  ${f.label}
                </button>
              `;
            })}
          </div>
          <${AgentsScreen}
            snapshot=${snapshot}
            query=${query}
            filter=${filter}
            order=${order}
            selectMode=${selectMode}
            selected=${selected}
            onAcceptArrivals=${function () { setOrder(sortedAgentNames(snapshot.agents)); }}
            onRowClick=${function (a) {
              if (selectMode) {
                var next = selected.slice();
                var i = next.indexOf(a.name);
                if (i === -1) next.push(a.name);
                else next.splice(i, 1);
                setSelected(next);
              } else {
                setDetail(a.name);
              }
            }}
          />
          ${selectMode &&
          selected.length > 0 &&
          html`
            <div class="actionbar">
              <span class="row-meta">${plural(selected.length, "selected", "selected")}</span>
              <span class="spacer"></span>
              <button class="btn" onClick=${function () {
                // ONE call, not one per agent: each backend call persists a
                // whole config snapshot, so N concurrent calls raced and the
                // last writer erased the other N-1 denials.
                call("ui_set_forbidden_batch", { agents: selected, denied: true });
                setSelectMode(false);
                setSelected([]);
              }}>Deny</button>
              <button class="btn danger" onClick=${function () { deleteAgents(selected); }}>
                Delete ${selected.length}
              </button>
            </div>
          `}
        `}

        ${screen === "agents" && detailAgent &&
        html`
          <div class="section">
            <button class="link" onClick=${function () { setDetail(null); }}>‹ Agents</button>
            <h2>Agent</h2>
          </div>
          <${AgentDetail}
            agent=${detailAgent}
            onSetCustom=${function (a, on) {
              call(on ? "ui_enable_custom" : "ui_disable_custom", { agent: a.name });
            }}
            onSetOverride=${function (a, jack, on) {
              call("ui_set_client_override", { agent: a.name, jack: jack, on: on });
            }}
            onResetCustom=${function (a) { call("ui_reset_custom_to_global", { agent: a.name }); }}
            onSetDenied=${function (a, denied) {
              call("ui_set_forbidden", { agent: a.name, denied: denied });
            }}
            onDelete=${function (a) { deleteAgents([a.name]); }}
          />
        `}

        ${screen === "settings" &&
        html`
          <div class="section"><h2>Settings</h2></div>
          <${SettingsScreen} snapshot=${snapshot} onToast=${setToast} />
        `}

        ${undo &&
        html`
          <div class="undobar">
            <span class="row-meta"
              >${plural(undo.count, "agent deleted", "agents deleted")}</span
            >
            <span class="spacer"></span>
            <button
              class="btn"
              onClick=${function () {
                var token = undo.token;
                setUndo(null);
                call("ui_undo", { token: token });
              }}
            >
              Undo
            </button>
            <button class="btn" onClick=${function () { setUndo(null); }}>Dismiss</button>
          </div>
        `}

        ${toast &&
        html`
          <div class="banner">
            <span class="banner-text">${toast}</span>
            <button class="btn" onClick=${function () { setToast(null); }}>Dismiss</button>
          </div>
        `}

        <${Footer}
          screen=${screen === "add" ? "jacks" : screen}
          agentCount=${snapshot.agents.length}
          onScreen=${function (k) {
            setScreen(k);
            setDetail(null);
            setQuery("");
          }}
        />
      </div>
    `;
  }

  render(h(App), document.getElementById("root"));
})();
