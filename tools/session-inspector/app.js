const elements = {
  storeTotal: document.querySelector("#store-total"),
  storeCount: document.querySelector("#store-count"),
  dataDir: document.querySelector("#data-dir"),
  filter: document.querySelector("#session-filter"),
  order: document.querySelector("#session-order"),
  select: document.querySelector("#session-select"),
  detail: document.querySelector("#session-detail"),
  analyze: document.querySelector("#analyze"),
  status: document.querySelector("#analysis-status"),
  statusLabel: document.querySelector("#status-label"),
  statusValue: document.querySelector("#status-value"),
  progress: document.querySelector("#status-progress"),
  error: document.querySelector("#error"),
  results: document.querySelector("#results"),
  scanTime: document.querySelector("#scan-time"),
  stats: document.querySelector("#stats"),
  categoryStack: document.querySelector("#category-stack"),
  categoryLegend: document.querySelector("#category-legend"),
  kindList: document.querySelector("#kind-list"),
  fieldPanel: document.querySelector("#field-panel"),
  pairingSection: document.querySelector("#pairing-section"),
  pairing: document.querySelector("#pairing"),
  largest: document.querySelector("#largest"),
};

const series = ["var(--series-1)", "var(--series-2)", "var(--series-3)", "var(--series-4)", "var(--series-5)", "var(--series-6)"];
let model = {
  sessions: [],
  selectedSessionId: undefined,
  selectedKind: undefined,
  filter: "",
  order: "size",
  summary: undefined,
  loading: false,
};

const formatBytes = (value) => {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let number = Number(value) || 0;
  let index = 0;
  while (number >= 1000 && index < units.length - 1) {
    number /= 1000;
    index += 1;
  }
  const digits = index === 0 || number >= 100 ? 0 : 2;
  return `${number.toFixed(digits)} ${units[index]}`;
};

const formatCount = (value) => new Intl.NumberFormat().format(value || 0);
const formatDate = (value) => new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value));

const update = (current, message) => {
  switch (message.type) {
    case "sessions-loaded":
      return {
        ...current,
        sessions: message.sessions,
        selectedSessionId: [...message.sessions].sort((left, right) => right.bytes - left.bytes)[0]?.id,
      };
    case "filter-changed":
      return { ...current, filter: message.value };
    case "order-changed":
      return { ...current, order: message.value };
    case "session-selected":
      return { ...current, selectedSessionId: message.id, summary: undefined, selectedKind: undefined };
    case "analysis-started":
      return { ...current, loading: true, summary: undefined, selectedKind: undefined };
    case "analysis-complete":
      return { ...current, loading: false, summary: message.summary, selectedKind: message.summary.kinds[0]?.kind };
    case "analysis-failed":
      return { ...current, loading: false };
    case "kind-selected":
      return { ...current, selectedKind: message.kind };
    default:
      return current;
  }
};

const dispatch = (message) => {
  model = update(model, message);
  render();
};

const orderedSessions = () => {
  const needle = model.filter.trim().toLowerCase();
  const sessions = model.sessions.filter((session) => session.id.toLowerCase().includes(needle));
  return sessions.sort((left, right) => {
    if (model.order === "newest") return right.mtimeMs - left.mtimeMs;
    if (model.order === "oldest") return left.mtimeMs - right.mtimeMs;
    return right.bytes - left.bytes;
  });
};

const element = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};

const renderSessionPicker = () => {
  const sessions = orderedSessions();
  const prior = elements.select.value;
  elements.select.replaceChildren(...sessions.map((session) => {
    const option = element("option", "", `${session.id}  ·  ${formatBytes(session.bytes)}  ·  ${formatDate(session.mtimeMs)}`);
    option.value = session.id;
    option.selected = session.id === model.selectedSessionId;
    return option;
  }));
  if (!elements.select.value && prior && sessions.some((session) => session.id === prior)) {
    elements.select.value = prior;
  }
  const selected = model.sessions.find((session) => session.id === model.selectedSessionId);
  elements.analyze.disabled = !selected || model.loading;
  elements.detail.textContent = selected
    ? `${selected.id}.jsonl · ${formatBytes(selected.bytes)} · modified ${formatDate(selected.mtimeMs)}`
    : `${sessions.length} matching sessions`;
};

const renderStats = (summary) => {
  const invalid = summary.invalidOuterRows + summary.invalidPayloadRows;
  const values = [
    [formatBytes(summary.source.bytes), "file size"],
    [formatCount(summary.rows), "persisted events"],
    [formatCount(summary.kinds.length), "event kinds"],
    [formatCount(invalid), "invalid records"],
  ];
  elements.stats.replaceChildren(...values.map(([value, label]) => {
    const card = element("div", "stat");
    card.append(element("span", "", label), element("strong", "", value));
    return card;
  }));
};

const renderCategories = (summary) => {
  const total = summary.categories.reduce((sum, item) => sum + item.rawBytes, 0) || 1;
  elements.categoryStack.replaceChildren(...summary.categories.map((item, index) => {
    const segment = element("div", "category-segment");
    segment.style.flexBasis = `${item.rawBytes / total * 100}%`;
    segment.style.background = series[index % series.length];
    segment.title = `${item.category}: ${formatBytes(item.rawBytes)}`;
    return segment;
  }));
  elements.categoryLegend.replaceChildren(...summary.categories.map((item, index) => {
    const legend = element("span", "legend-item");
    const swatch = element("span", "swatch");
    swatch.style.background = series[index % series.length];
    legend.append(swatch, document.createTextNode(`${item.category} · ${formatBytes(item.rawBytes)} · ${(item.rawBytes / total * 100).toFixed(1)}%`));
    return legend;
  }));
};

const renderFields = (kind) => {
  elements.fieldPanel.replaceChildren();
  if (!kind) {
    elements.fieldPanel.append(element("p", "muted", "No body fields found."));
    return;
  }
  const heading = element("div", "field-heading");
  heading.append(element("code", "", kind.kind), element("span", "muted", `${formatCount(kind.rows)} rows`));
  const list = element("div", "field-list");
  const maximum = kind.fields[0]?.bytes || 1;
  for (const field of kind.fields.slice(0, 14)) {
    const row = element("div");
    const meta = element("div", "field-meta");
    meta.append(element("code", "", field.field), element("span", "", formatBytes(field.bytes)));
    const track = element("span", "bar-track");
    const fill = element("span", "bar-fill");
    fill.style.width = `${Math.max(.4, field.bytes / maximum * 100)}%`;
    track.append(fill);
    row.append(meta, track);
    list.append(row);
  }
  elements.fieldPanel.append(heading, list);
};

const renderKinds = (summary) => {
  const top = summary.kinds.slice(0, 24);
  const maximum = top[0]?.rawBytes || 1;
  elements.kindList.replaceChildren(...top.map((kind) => {
    const button = element("button", "kind-button");
    button.type = "button";
    button.dataset.kind = kind.kind;
    button.setAttribute("aria-pressed", kind.kind === model.selectedKind ? "true" : "false");
    const track = element("span", "bar-track");
    const fill = element("span", "bar-fill");
    fill.style.width = `${Math.max(.4, kind.rawBytes / maximum * 100)}%`;
    track.append(fill);
    button.append(element("code", "", kind.kind), track, element("span", "kind-size", formatBytes(kind.rawBytes)));
    button.addEventListener("click", () => dispatch({ type: "kind-selected", kind: kind.kind }));
    return button;
  }));
  renderFields(summary.kinds.find((kind) => kind.kind === model.selectedKind));
};

const renderPairing = (summary) => {
  const visible = summary.pairing.filter((pair) => pair.leftCount || pair.rightCount);
  elements.pairingSection.hidden = visible.length === 0;
  elements.pairing.replaceChildren(...visible.map((pair) => {
    const card = element("article", "pair-card");
    const flow = element("div", "pair-flow");
    const side = (name, count, unique) => {
      const node = element("div", "pair-side");
      node.append(element("strong", "", name), element("small", "", `${formatCount(count)} copies · ${formatCount(unique)} distinct values`));
      return node;
    };
    flow.append(side(pair.left, pair.leftCount, pair.leftUnique), element("span", "pair-arrow", "⇄"), side(pair.right, pair.rightCount, pair.rightUnique));
    const result = element("p", "pair-result");
    result.append(element("strong", "", `${formatCount(pair.matched)} exact matched occurrences`), document.createTextNode(` carrying ${formatBytes(pair.matchedBytesEachSide)} on each side · ${formatBytes(pair.matchedBytesEachSide * 2)} across the pair.`));
    card.append(flow, result);
    return card;
  }));
};

const renderLargest = (summary) => {
  elements.largest.replaceChildren(...summary.largest.slice(0, 20).map((item) => {
    const row = document.createElement("tr");
    const line = element("td", "numeric", formatCount(item.line));
    const kind = document.createElement("td");
    kind.append(element("code", "", item.kind));
    const size = element("td", "numeric", formatBytes(item.bytes));
    const fields = document.createElement("td");
    fields.append(element("code", "", item.fields.join(" · ")));
    row.append(line, kind, size, fields);
    return row;
  }));
};

const render = () => {
  renderSessionPicker();
  const summary = model.summary;
  elements.results.hidden = !summary;
  if (!summary) return;
  elements.scanTime.textContent = `Analyzed in ${(summary.elapsedMs / 1000).toFixed(1)} s`;
  renderStats(summary);
  renderCategories(summary);
  renderKinds(summary);
  renderPairing(summary);
  renderLargest(summary);
};

const setError = (message) => {
  elements.error.hidden = !message;
  elements.error.textContent = message || "";
};

const pollJob = async (jobId) => {
  while (true) {
    const response = await fetch(`/api/jobs/${encodeURIComponent(jobId)}`);
    if (!response.ok) throw new Error((await response.json()).error || "Could not read analysis job");
    const { job } = await response.json();
    const fraction = job.totalBytes ? Math.min(1, job.bytesRead / job.totalBytes) : 0;
    elements.status.hidden = false;
    elements.statusLabel.textContent = job.status === "queued" ? "Waiting for current scan" : `Reading ${formatCount(job.rows)} events`;
    elements.statusValue.textContent = `${(fraction * 100).toFixed(1)}% · ${formatBytes(job.bytesRead)} / ${formatBytes(job.totalBytes)}`;
    elements.progress.value = fraction;
    if (job.status === "complete") return job.summary;
    if (job.status === "error") throw new Error(job.error || "Analysis failed");
    await new Promise((resolve) => setTimeout(resolve, 400));
  }
};

const analyzeSelected = async () => {
  if (!model.selectedSessionId) return;
  setError();
  dispatch({ type: "analysis-started" });
  elements.status.hidden = false;
  elements.statusLabel.textContent = "Starting analysis";
  elements.statusValue.textContent = "0%";
  elements.progress.value = 0;
  try {
    const response = await fetch(`/api/sessions/${encodeURIComponent(model.selectedSessionId)}/analyze`, { method: "POST" });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || "Could not start analysis");
    const summary = await pollJob(payload.job.id);
    elements.status.hidden = true;
    dispatch({ type: "analysis-complete", summary });
  } catch (error) {
    elements.status.hidden = true;
    dispatch({ type: "analysis-failed" });
    setError(error instanceof Error ? error.message : String(error));
  }
};

elements.filter.addEventListener("input", () => dispatch({ type: "filter-changed", value: elements.filter.value }));
elements.order.addEventListener("change", () => dispatch({ type: "order-changed", value: elements.order.value }));
elements.select.addEventListener("change", () => dispatch({ type: "session-selected", id: elements.select.value }));
elements.analyze.addEventListener("click", analyzeSelected);

const load = async () => {
  try {
    const response = await fetch("/api/sessions");
    if (!response.ok) throw new Error("Could not list sessions");
    const payload = await response.json();
    const total = payload.sessions.reduce((sum, session) => sum + session.bytes, 0);
    elements.storeTotal.textContent = formatBytes(total);
    elements.storeCount.textContent = `${formatCount(payload.sessions.length)} files`;
    elements.dataDir.textContent = payload.dataDir;
    dispatch({ type: "sessions-loaded", sessions: payload.sessions });
  } catch (error) {
    setError(error instanceof Error ? error.message : String(error));
  }
};

load();
