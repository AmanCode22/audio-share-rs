// AudioShare Companion UI — talks to the Tauri backend via invoke()/events.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// ---------- tabs ----------
function selectTab(which) {
  const server = which === "server";
  $("tab-server").classList.toggle("active", server);
  $("tab-client").classList.toggle("active", !server);
  $("pane-server").classList.toggle("active", server);
  $("pane-client").classList.toggle("active", !server);
}
$("tab-server").onclick = () => selectTab("server");
$("tab-client").onclick = () => selectTab("client");

// ---------- helpers ----------
function fmtBytes(n) {
  if (!n && n !== 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

// ---------- server pane ----------
let lanAddrs = [];
async function loadAddresses() {
  try {
    lanAddrs = await invoke("lan_address_list");
    const sel = $("srv-host");
    sel.innerHTML = "";
    const any = document.createElement("option");
    any.value = "0.0.0.0";
    any.textContent = "All interfaces (0.0.0.0)";
    sel.appendChild(any);
    for (const a of lanAddrs) {
      const o = document.createElement("option");
      o.value = a;
      o.textContent = a;
      sel.appendChild(o);
    }
    const priv = lanAddrs.find((a) =>
      /^(10\.|172\.(1[6-9]|2\d|3[01])\.|192\.168\.)/.test(a)
    );
    if (priv) sel.value = priv;
  } catch (e) {
    console.warn("lan addresses failed", e);
  }
}

async function loadDevices() {
  try {
    const devices = await invoke("list_capture_devices");
    const sel = $("srv-device");
    sel.innerHTML = "";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = "Default device";
    sel.appendChild(def);
    for (const d of devices) {
      const o = document.createElement("option");
      o.value = d.name;
      o.textContent = `${d.is_default ? "[default] " : ""}${d.name}${
        d.sample_rate ? ` (${d.sample_rate} Hz, ${d.channels} ch)` : ""
      }`;
      sel.appendChild(o);
    }
    const outputs = await invoke("list_output_devices_cmd");
    const osel = $("cli-device");
    osel.innerHTML = "";
    const odef = document.createElement("option");
    odef.value = "";
    odef.textContent = "System default";
    osel.appendChild(odef);
    for (const d of outputs) {
      const o = document.createElement("option");
      o.value = d.name;
      o.textContent = `${d.is_default ? "[default] " : ""}${d.name}`;
      osel.appendChild(o);
    }
  } catch (e) {
    console.warn("device list failed", e);
  }
}

$("srv-block").oninput = (e) => ($("srv-block-label").textContent = `${e.target.value} ms`);
$("cli-buffer").oninput = (e) => ($("cli-buffer-label").textContent = `${e.target.value} ms`);
$("cli-volume").oninput = (e) => ($("cli-volume-label").textContent = `${e.target.value}%`);

$("srv-start").onclick = async () => {
  $("srv-start").disabled = true;
  try {
    await invoke("server_start", {
      opts: {
        host: $("srv-host").value === "0.0.0.0" ? null : $("srv-host").value,
        port: parseInt($("srv-port").value, 10),
        source: $("srv-source").value,
        device: $("srv-device").value || null,
        encoding: $("srv-encoding").value,
        blockMs: parseInt($("srv-block").value, 10),
        name: $("srv-name").value,
        mdns: $("srv-mdns").checked,
      },
    });
  } catch (e) {
    $("subtitle").textContent = `error: ${e}`;
  }
  $("srv-start").disabled = false;
};

$("srv-stop").onclick = () => invoke("server_stop").catch(console.warn);

// ---------- client pane ----------
$("scan-btn").onclick = async () => {
  $("scan-status").textContent = "scanning…";
  $("scan-btn").disabled = true;
  try {
    const found = await invoke("discover_servers", { timeoutMs: 3000 });
    const table = $("mdns-table");
    const rows = $("mdns-rows");
    rows.innerHTML = "";
    table.hidden = found.length === 0;
    for (const d of found) {
      const tr = document.createElement("tr");
      tr.innerHTML = `<td>${d.name}</td><td>${d.host}:${d.port}</td>`;
      const td = document.createElement("td");
      const btn = document.createElement("button");
      btn.className = "btn flat";
      btn.textContent = "Use";
      btn.onclick = () => {
        $("cli-host").value = d.host;
        $("cli-port").value = d.port;
      };
      td.appendChild(btn);
      tr.appendChild(td);
      rows.appendChild(tr);
    }
    $("scan-status").textContent = found.length ? `${found.length} found` : "none found";
  } catch (e) {
    $("scan-status").textContent = `scan failed: ${e}`;
  }
  $("scan-btn").disabled = false;
};

$("cli-start").onclick = async () => {
  $("cli-start").disabled = true;
  try {
    await invoke("client_start", {
      opts: {
        host: $("cli-host").value.trim(),
        port: parseInt($("cli-port").value, 10),
        tcpData: $("cli-tcp").checked,
        outputDevice: $("cli-device").value || null,
        bufferMs: parseInt($("cli-buffer").value, 10),
        volume: parseInt($("cli-volume").value, 10) / 100,
      },
    });
  } catch (e) {
    $("cl-msg").textContent = `error: ${e}`;
  }
  $("cli-start").disabled = false;
};

$("cli-stop").onclick = () => invoke("client_stop").catch(console.warn);

// ---------- live stats ----------
listen("tick", (ev) => {
  const { server, client } = ev.payload;

  if (server.running) {
    $("st-format").textContent = server.format || "—";
    $("st-clients").textContent = server.clients.length;
    $("st-bytes").textContent = fmtBytes(server.total_bytes);
    $("st-drops").textContent = server.udp_drops;
    const rows = $("peer-rows");
    rows.innerHTML = "";
    $("peer-table").hidden = server.clients.length === 0;
    for (const c of server.clients) {
      const tr = document.createElement("tr");
      tr.innerHTML = `<td>${c.addr}</td><td>${c.tcp_data ? "TCP" : "UDP"}</td><td>${
        c.rtt_ms != null ? c.rtt_ms + " ms" : "—"
      }</td><td>${fmtBytes(c.sent_bytes)}</td>`;
      rows.appendChild(tr);
    }
    $("subtitle").textContent = `sharing on ${server.bind_addr}`;
    $("conn-pill").textContent = `${server.clients.length} listening`;
    $("conn-pill").className = "pill " + (server.clients.length ? "ok" : "");
    $("connect-hint").textContent = lanAddrs.length
      ? `Players should connect to ${lanAddrs[0]}:${server.port} (or use LAN discovery).`
      : "";
    $("srv-stop").disabled = false;
  } else {
    $("st-format").textContent = "—";
    $("st-clients").textContent = "0";
    $("st-bytes").textContent = "0 B";
    $("st-drops").textContent = "0";
    $("peer-table").hidden = true;
    $("subtitle").textContent = "not sharing";
    $("conn-pill").textContent = "idle";
    $("conn-pill").className = "pill";
    $("connect-hint").textContent = "";
    $("srv-stop").disabled = true;
  }

  if (client.connected) {
    $("cl-buffer").textContent = `${client.buffered_ms} ms`;
    $("cl-recv").textContent = fmtBytes(client.bytes_received);
    $("cl-loss").textContent = client.lost_packets;
    $("cl-rtt").textContent = client.rtt_ms != null ? `${client.rtt_ms} ms` : "—";
    $("cl-msg").textContent = `playing from “${client.server_name}” (${client.format})` +
      (client.reconnects ? ` · reconnects: ${client.reconnects}` : "");
    $("conn-pill").textContent = "playing";
    $("conn-pill").className = "pill ok";
    $("cli-stop").disabled = false;
    $("subtitle").textContent = `playing from ${client.server_name}`;
  } else {
    $("cl-buffer").textContent = "—";
    $("cl-loss").textContent = "0";
    $("cl-rtt").textContent = "—";
    $("cli-stop").disabled = true;
    if (client.last_error && !$("cli-start").disabled) {
      $("cl-msg").textContent = `reconnecting… (${client.last_error})`;
    } else if (!$("cli-start").disabled) {
      $("cl-msg").textContent = "";
    }
    if ($("conn-pill").textContent !== "idle" && !server.running) {
      $("conn-pill").textContent = "idle";
      $("conn-pill").className = "pill";
    }
  }
});

// ---------- init ----------
loadAddresses();
loadDevices();
