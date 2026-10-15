// Server-rendered HTML: the home page, the page where you paste your aiva token, and errors.
// Self-contained (no external fonts, scripts or images) so it loads instantly and the CSP stays strict.

import { esc, html } from "./util";

const STYLE = `
:root{
  --paper:#f3f1ec;--sheet:#fbfaf7;--ink:#151513;--ink-2:#55534d;--ink-3:#8b887f;--rule:#dedad1;--rule-2:#cbc6ba;
  --signal:#d9481c;--ok:#1d6b45;--bad:#b3261e;--field:#ffffff;
  --serif:ui-serif,"New York","Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;
  --sans:-apple-system,BlinkMacSystemFont,"SF Pro Text","Helvetica Neue",Helvetica,Arial,sans-serif;
  --mono:ui-monospace,"SF Mono","JetBrains Mono",Menlo,Consolas,monospace;
}
@media (prefers-color-scheme:dark){:root{
  --paper:#0f0f0e;--sheet:#161614;--ink:#eceae4;--ink-2:#a9a69d;--ink-3:#75736b;--rule:#2a2926;--rule-2:#3a3934;
  --signal:#ff6a3d;--ok:#4cc58a;--bad:#ff7a6e;--field:#0c0c0b;
}}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.55 var(--sans);-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}
::selection{background:var(--ink);color:var(--paper)}
a{color:inherit;text-underline-offset:3px;text-decoration-thickness:1px}
.wrap{max-width:640px;margin:0 auto;padding:0 24px;min-height:100vh;display:flex;flex-direction:column}
header{display:flex;align-items:center;justify-content:space-between;padding:28px 0 0}
.mark{display:inline-flex;align-items:center;gap:9px;font:600 15px/1 var(--sans);letter-spacing:-.01em;text-decoration:none}
.mark i{width:9px;height:9px;border-radius:50%;background:var(--ink);display:inline-block}
.mark i.live{background:var(--signal)}
header small{font:12px/1 var(--mono);color:var(--ink-3);letter-spacing:.02em}
main{flex:1;padding:12vh 0 64px}
.eyebrow{font:500 11px/1 var(--mono);letter-spacing:.14em;text-transform:uppercase;color:var(--ink-3);margin:0 0 22px}
h1{font:400 clamp(34px,6vw,46px)/1.06 var(--serif);letter-spacing:-.022em;margin:0 0 18px}
h1 em{font-style:italic}
.lede{font-size:17px;line-height:1.55;color:var(--ink-2);margin:0 0 40px;max-width:31em}
code{font:13px/1.4 var(--mono);background:transparent;border:1px solid var(--rule);border-radius:5px;padding:1px 5px;color:var(--ink)}
.route{display:grid;grid-template-columns:auto 1fr auto 1fr auto;align-items:center;gap:12px;margin:0 0 36px;font:12px/1.3 var(--mono);color:var(--ink-2)}
.route .node{display:flex;flex-direction:column;gap:5px}
.route .node b{font-weight:500;color:var(--ink)}
.route .node span{color:var(--ink-3)}
.route .node:last-child{text-align:right}
.route .wire{height:1px;background:repeating-linear-gradient(90deg,var(--rule-2) 0 4px,transparent 4px 8px)}
.route.ready .wire{background:var(--ok)}
label.k{display:flex;justify-content:space-between;align-items:baseline;font:500 12px/1 var(--mono);letter-spacing:.06em;text-transform:uppercase;color:var(--ink-3);margin:0 0 10px}
label.k button{all:unset;cursor:pointer;font:inherit;color:var(--ink-2);text-transform:none;letter-spacing:0}
label.k button:hover{color:var(--ink)}
.field{position:relative}
.field input{width:100%;font:15px/1 var(--mono);letter-spacing:.01em;color:var(--ink);background:var(--field);border:1px solid var(--rule-2);border-radius:10px;padding:17px 16px;outline:none;transition:border-color .15s,box-shadow .15s}
.field input::placeholder{color:var(--ink-3)}
.field input:focus{border-color:var(--ink);box-shadow:0 0 0 4px color-mix(in srgb,var(--ink) 8%,transparent)}
.field.ok input{border-color:var(--ok)}
.field.bad input{border-color:var(--bad)}
.hint{min-height:22px;margin:10px 0 28px;font:13px/1.5 var(--sans);color:var(--ink-3);display:flex;gap:8px;align-items:baseline}
.hint .dot{width:6px;height:6px;border-radius:50%;background:currentColor;flex:none;transform:translateY(-1px)}
.hint.ok{color:var(--ok)}.hint.bad{color:var(--bad)}
.actions{display:flex;gap:10px;align-items:center}
.btn{appearance:none;border:1px solid var(--ink);background:var(--ink);color:var(--paper);font:500 15px/1 var(--sans);letter-spacing:-.005em;padding:15px 22px;border-radius:10px;cursor:pointer;transition:opacity .15s,transform .05s}
.btn:hover{opacity:.88}.btn:active{transform:translateY(1px)}
.btn[disabled]{opacity:.35;cursor:default}
.btn.ghost{background:transparent;color:var(--ink-2);border-color:transparent}
.btn.ghost:hover{color:var(--ink);opacity:1}
.btn.wide{flex:1}
.fine{margin:44px 0 0;padding-top:20px;border-top:1px solid var(--rule);font-size:13px;color:var(--ink-3);display:grid;grid-template-columns:1fr 1fr;gap:16px}
.fine b{display:block;font-weight:500;color:var(--ink-2);margin-bottom:2px}
.cmd{display:flex;align-items:center;justify-content:space-between;gap:16px;background:var(--sheet);border:1px solid var(--rule);border-radius:12px;padding:6px 6px 6px 18px;margin:0 0 56px}
.cmd span{font:14px/1.4 var(--mono);overflow-x:auto;white-space:nowrap;scrollbar-width:none}
.cmd span::before{content:"$ ";color:var(--ink-3)}
.cmd .btn{padding:11px 16px;font-size:14px}
.cmd.ps span::before{content:"> "}
.cmd:has(+ .os){margin-bottom:28px}
.os{font:500 11px/1 var(--mono);letter-spacing:.12em;text-transform:uppercase;color:var(--ink-3);margin:0 0 10px}
ol.steps{list-style:none;margin:0;padding:0;border-top:1px solid var(--rule)}
ol.steps li{display:grid;grid-template-columns:56px 1fr;gap:8px;padding:20px 0;border-bottom:1px solid var(--rule)}
ol.steps li::before{content:counter(s,decimal-leading-zero);counter-increment:s;font:12px/1.9 var(--mono);color:var(--ink-3)}
ol.steps{counter-reset:s}
ol.steps b{font-weight:500}
ol.steps p{margin:2px 0 0;color:var(--ink-2)}
footer{padding:28px 0 32px;font:12px/1.5 var(--mono);color:var(--ink-3);display:flex;justify-content:space-between;gap:16px;flex-wrap:wrap}
.err{border-left:2px solid var(--bad);padding:2px 0 2px 14px;color:var(--ink-2);margin:0 0 28px}
@media (max-width:520px){main{padding-top:7vh}.fine{grid-template-columns:1fr}.route{gap:8px;font-size:11px}.actions{flex-direction:column-reverse;align-items:stretch}}
@media (prefers-reduced-motion:reduce){*{transition:none!important}}
`;

const FAVICON =
  "data:image/svg+xml," +
  encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><circle cx="16" cy="16" r="9" fill="#d9481c"/></svg>');

function respond(title: string, body: string, opts: { status?: number; script?: string; live?: boolean } = {}): Response {
  const nonce = crypto.randomUUID().replace(/-/g, "");
  return html(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="light dark"><title>${esc(title)}</title><link rel="icon" href="${FAVICON}"><style>${STYLE}</style></head>
<body><div class="wrap"><header><a class="mark" href="/"><i${opts.live ? ' class="live"' : ""}></i>aiva</a><small>no account · nothing stored</small></header>
<main>${body}</main>
<footer><span>aiva relays traffic between your apps and your computer.</span><span>Your computer connects out. No open ports.</span></footer></div>
${opts.script ? `<script nonce="${nonce}">${opts.script}</script>` : ""}</body></html>`,
    nonce,
    opts.status ?? 200,
  );
}

export function message(title: string, body: string, status = 400): Response {
  return respond(`${title} · aiva`, `<p class="eyebrow">Something's off</p><h1>${esc(title)}</h1><p class="lede">${esc(body)}</p>`, { status });
}

const COPY_SCRIPT = `
document.querySelectorAll("[data-copy]").forEach(function(b){b.addEventListener("click",function(){
  navigator.clipboard.writeText(b.dataset.copy).then(function(){var t=b.textContent;b.textContent="Copied";setTimeout(function(){b.textContent=t},1400)});
})});`;

export function home(origin: string): Response {
  const install = `curl -fsSL ${origin}/install.sh | sh`;
  // Works in both PowerShell and Command Prompt.
  const installWindows = `powershell -ExecutionPolicy Bypass -c "irm ${origin}/install.ps1 | iex"`;
  return respond(
    "aiva — talk to your code from anywhere",
    `<p class="eyebrow">MCP connector for your own computer</p>
<h1>Talk to your code<br>from <em>anywhere</em>.</h1>
<p class="lede">aiva lets Claude, ChatGPT and other AI apps read the folders you choose on your computer, and edit them if you allow it. Ask about your project from your phone, out loud, while you're away from your desk.</p>
<p class="os">macOS and Linux · Terminal</p>
<div class="cmd"><span>${esc(install)}</span><button class="btn" data-copy="${esc(install)}">Copy</button></div>
<p class="os">Windows · PowerShell or Command Prompt</p>
<div class="cmd ps"><span>${esc(installWindows)}</span><button class="btn" data-copy="${esc(installWindows)}">Copy</button></div>
<ol class="steps">
<li><div><b>Install and pick folders.</b><p>The installer starts <code>aiva setup</code>, which prints a URL and a token.</p></div></li>
<li><div><b>Add the URL as a connector.</b><p>In Claude: Settings → Connectors → Add custom connector. ChatGPT, Cursor and Claude Code work the same way.</p></div></li>
<li><div><b>Paste the token.</b><p>Click Connect and paste it on the page that opens. That's the whole setup.</p></div></li>
</ol>
<div class="fine"><div><b>Read-only by default</b>Edits are opt-in per folder, every change can be undone, and secret files like <code>.env</code> are never touched.</div><div><b>Nothing to sign up for</b>Your computer makes its own key. This server only checks it and passes traffic along.</div></div>`,
    { script: COPY_SCRIPT, live: true },
  );
}

/** "Claude (claude.ai)" → ["Claude", "claude.ai"] */
function splitApp(app: string): [string, string] {
  const m = app.match(/^(.*) \(([^)]*)\)$/);
  return m ? [m[1], m[2]] : [app, ""];
}

export function connectPage(opts: { pending: string; app: string; url: string | null; error?: string }): Response {
  const [appName, appHost] = splitApp(opts.app);
  const id = opts.url?.match(/\/([a-f0-9]{20})\/mcp$/)?.[1] ?? "";
  const shortId = id ? `${id.slice(0, 6)}…${id.slice(-4)}` : "your computer";
  // Checks the pasted token against the connector URL in the browser (SHA-256, first 20 hex),
  // so a wrong token is caught before anything is sent.
  const script = `
var form=document.getElementById("f"),input=document.getElementById("token"),field=input.parentNode,hint=document.getElementById("hint"),
    go=document.getElementById("go"),route=document.getElementById("route"),want=form.dataset.id,toggle=document.getElementById("show");
function set(state,text){field.className="field"+(state?" "+state:"");hint.className="hint"+(state?" "+state:"");hint.innerHTML=(state?'<span class="dot"></span>':"")+text;
  route.className="route"+(state==="ok"?" ready":"");go.disabled=state==="bad"}
async function check(){var v=input.value.trim();
  if(!v){set("","Run <code>aiva token</code> on that computer to see it.");return}
  if(!/^aiva_[A-Za-z0-9_-]{40,64}$/.test(v)){set("bad","That isn't an aiva token. It starts with <code>aiva_</code>.");return}
  if(!want||!crypto.subtle){set("ok","Looks right.");return}
  var d=await crypto.subtle.digest("SHA-256",new TextEncoder().encode(v));
  var h=Array.from(new Uint8Array(d)).map(function(b){return b.toString(16).padStart(2,"0")}).join("").slice(0,20);
  if(h===want)set("ok","Matches this connector. Ready to connect.");else set("bad","This token belongs to a different computer.")}
input.addEventListener("input",check);input.addEventListener("paste",function(){setTimeout(check,0)});
toggle.addEventListener("click",function(){var s=input.type==="password";input.type=s?"text":"password";toggle.textContent=s?"Hide":"Show";input.focus()});
form.addEventListener("submit",function(e){if(e.submitter&&e.submitter.value==="deny")return;go.disabled=true;go.textContent="Connecting…"});
check();`;
  return respond(
    `Connect ${appName} · aiva`,
    `<p class="eyebrow">Connection request</p>
<h1>Connect <em>${esc(appName)}</em><br>to your computer.</h1>
<p class="lede">Paste the token that <code>aiva setup</code> printed. ${esc(appName)} will be able to use the folders you shared on that machine, nothing else.</p>
<div class="route" id="route"><div class="node"><b>${esc(appName)}</b><span>${esc(appHost || "app")}</span></div><div class="wire"></div>
<div class="node" style="text-align:center"><b>aiva</b><span>relay</span></div><div class="wire"></div>
<div class="node"><b>${esc(shortId)}</b><span>your computer</span></div></div>
${opts.error ? `<p class="err">${esc(opts.error)}</p>` : ""}
<form id="f" method="post" action="/oauth/approve" data-id="${esc(id)}">
<input type="hidden" name="pending" value="${esc(opts.pending)}">
<label class="k" for="token">Token <button type="button" id="show">Show</button></label>
<div class="field"><input id="token" name="token" type="password" placeholder="aiva_" autocomplete="off" autocapitalize="off" spellcheck="false" autofocus required></div>
<p class="hint" id="hint" aria-live="polite"></p>
<div class="actions"><button class="btn ghost" name="action" value="deny" formnovalidate>Cancel</button><button class="btn wide" id="go" name="action" value="allow">Connect</button></div>
</form>
<div class="fine"><div><b>Checked, then forgotten</b>The token proves this URL is yours. It isn't stored anywhere on this server.</div><div><b>Change your mind later</b>Run <code>aiva rotate</code> to disconnect every app at once.</div></div>`,
    { status: opts.error ? 400 : 200, script },
  );
}
