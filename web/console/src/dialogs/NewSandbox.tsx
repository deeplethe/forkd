// ---- new sandbox ----------------------------------------------------------------
// The master places it on a node with room, unless one is picked. Its token
// is shown once, here, because the API returns it only at creation.
import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { CreatedSandbox, TemplateEntry } from "../types";
import { api } from "../api";
import { st } from "../state";
import { Dropdown, type Option } from "../components/Dropdown";
import { useEscape } from "../components/hooks";

const ANY_NODE: Option = { value: "", label: "Any node with room" };

export function NewSandbox({ open, seq, onClose, onCreated }: { open: boolean; seq: number; onClose: () => void; onCreated: () => void }) {
  const [tpls, setTpls] = useState<Option[]>([]), [tpl, setTpl] = useState("");
  const [nodes, setNodes] = useState<Option[]>([ANY_NODE]), [node, setNode] = useState("");
  const [port, setPort] = useState("");
  const [err, setErr] = useState<ReactNode>("");
  const [out, setOut] = useState<{ ok: string; token: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [copy, setCopy] = useState("Copy");
  const copyT = useRef(0);
  useEscape(open, onClose);

  // Each opening starts clean and asks for the templates afresh.
  useEffect(() => {
    if (!seq) return;
    setErr(""); setOut(null);
    (async () => {
      try {
        const list = await api<TemplateEntry[]>("/v1/templates");
        const o = list.length ? list.map((t) => { const n = typeof t === "string" ? t : t.name; return { value: n, label: n }; }) : [{ value: "", label: "No templates registered" }];
        setTpls(o); setTpl(o[0].value);
        setNodes([ANY_NODE, ...(st.NOW ? st.NOW.nodes.filter((n) => n.connected).map((n) => ({ value: n.id, label: n.id })) : [])]); setNode("");
      } catch (e) { setErr(String((e as Error).message || e)); }
    })();
  }, [seq]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setErr(""); setOut(null);
    const body: { template: string; node_id?: string; port?: number } = { template: tpl };
    if (!body.template) { setErr("Register a template first (POST /v1/templates)."); return; }
    if (node) body.node_id = node;
    if (port) body.port = Number(port);
    setBusy(true);
    try {
      const r = await api<CreatedSandbox | CreatedSandbox[]>("/v1/sandboxes", body);
      const v = Array.isArray(r) ? r[0] : r;
      setOut({ ok: `Created ${v.id} on ${v.node_id}` + (v.gateway_port ? `, reachable on port ${v.gateway_port} of that node.` : "."), token: v.token || "(no token returned)" });
      onCreated();
    } catch (x) {
      setErr(<><b>The master did not create it.</b><details open><summary>Details</summary><code>{String((x as Error).message || x)}</code></details></>);
    } finally { setBusy(false); }
  };
  const copyToken = async () => {
    try { await navigator.clipboard.writeText(out?.token || ""); setCopy("Copied"); } catch { setCopy("Select and copy"); }
    clearTimeout(copyT.current);
    copyT.current = window.setTimeout(() => setCopy("Copy"), 2000);
  };

  return createPortal(
    <div className="gate" id="newDlg" hidden={!open} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={submit}>
        <h1>New sandbox</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <p>The master places it on a node with room, unless you pick one.</p>
        <label htmlFor="newTpl">Template</label><Dropdown id="newTpl" options={tpls} value={tpl} onChange={setTpl} />
        <label htmlFor="newNode">Node</label><Dropdown id="newNode" options={nodes} value={node} onChange={setNode} />
        <label htmlFor="newPort">Port inside the sandbox</label><input id="newPort" type="number" min="1" max="65535" placeholder="the template's default" value={port} onChange={(e) => setPort(e.target.value)} />
        <button type="submit" className="go" disabled={busy}>Create it</button>
        <div hidden={!out}>
          <div className="ok">{out?.ok}</div>
          <div className="note">Its token is shown once. Requests to the sandbox's gateway port carry it in <code>x-sandstorm-sandbox-token</code>.</div>
          <div className="secret"><code>{out?.token}</code><button type="button" onClick={copyToken}>{copy}</button></div>
        </div>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
