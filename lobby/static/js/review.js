// The review tab of a room: what the filters flag in its YAMLs, the filters, and their editor.
// Everything comes from /api/room/<id>/review, which is loaded again after every change.
"use strict";

(() => {
    const root = document.getElementById("review");
    const roomId = root.dataset.roomId;
    const api = `/api/room/${roomId}/review`;

    const SEVERITY_RANK = { info: 0, warning: 1, error: 2 };

    // Which check types a condition can use, and what they need besides the option
    const CHECKS = [
        { type: "truthy", label: "is on" },
        { type: "not_truthy", label: "is off" },
        { type: "equals", label: "equals", value: "text" },
        { type: "not_equals", label: "doesn't equal", value: "text" },
        { type: "greater_than", label: "is more than", value: "number" },
        { type: "less_than", label: "is less than", value: "number" },
        { type: "range", label: "is between", value: "range" },
        { type: "contains", label: "contains", value: "text" },
        { type: "not_contains", label: "doesn't contain", value: "text" },
        { type: "regex", label: "matches the regex", value: "pattern" },
        { type: "count", label: "has a number of entries that", value: "count" },
        { type: "exists", label: "is set" },
        { type: "not_exists", label: "isn't set" },
    ];
    // What the number of entries of `count` can be compared with
    const COUNT_CHECKS = ["equals", "not_equals", "greater_than", "less_than", "range"];

    const GROUPS = [
        { type: "check", label: "Option" },
        { type: "and", label: "All of" },
        { type: "or", label: "Any of" },
        { type: "at_least", label: "At least" },
        { type: "not", label: "Not" },
    ];

    let state = { builtins: [], filters: [], yamls: [] };

    // == Helpers ==

    function el(tag, props = {}, ...children) {
        const elmt = document.createElement(tag);
        for (const [key, value] of Object.entries(props)) {
            if (value === undefined || value === null || value === false) {
                continue;
            }
            if (key === "class") {
                elmt.className = value;
            } else if (key === "text") {
                elmt.textContent = value;
            } else if (key.startsWith("on")) {
                elmt.addEventListener(key.slice(2), value);
            } else if (key in elmt && typeof value !== "string") {
                elmt[key] = value;
            } else {
                elmt.setAttribute(key, value === true ? "" : value);
            }
        }
        for (const child of children.flat()) {
            if (child !== null && child !== undefined && child !== false) {
                elmt.append(child);
            }
        }
        return elmt;
    }

    async function request(method, url, body) {
        const options = { method, headers: {} };
        if (body !== undefined) {
            options.headers["Content-Type"] = "application/json";
            options.body = typeof body === "string" ? body : JSON.stringify(body);
        }
        const response = await fetch(url, options);
        if (!response.ok) {
            const text = await response.text().catch(() => "");
            throw new Error(text || `${response.status} ${response.statusText}`);
        }
        const type = response.headers.get("content-type") || "";
        return type.includes("json") ? response.json() : null;
    }

    function notify(message, kind = "error") {
        const messages = document.getElementById("messages");
        const elmt = el("pre", { class: kind, text: message });
        messages.append(elmt);
        setTimeout(() => elmt.remove(), 6000);
    }

    // Runs a change, then shows the tab as it is after it. Changes run one after the other,
    // each after the reload of the one before, so that a change made from what the page
    // shows is made from what the server has.
    let changes = Promise.resolve();
    function change(action) {
        changes = changes.then(async () => {
            try {
                await action();
            } catch (e) {
                notify(e.message);
            }
            await load();
        });
        return changes;
    }

    function formatTime(naive) {
        const time = Date.parse(naive + "Z");
        if (isNaN(time)) {
            return naive;
        }
        return new Intl.DateTimeFormat("default", { dateStyle: "medium", timeStyle: "short" }).format(time);
    }

    function severityChip(severity) {
        return el("span", { class: `review-severity review-severity-${severity}`, text: severity });
    }

    // Only the latest load is shown: an earlier one coming back after it has older state
    let latestLoad = 0;
    async function load() {
        const thisLoad = ++latestLoad;
        let loaded;
        try {
            loaded = await request("GET", api);
        } catch (e) {
            if (thisLoad === latestLoad) {
                notify("Couldn't load the review: " + e.message);
            }
            return;
        }
        if (thisLoad !== latestLoad) {
            return;
        }
        state = loaded;
        render();
    }

    function render() {
        renderResults();
        renderCustomFilters();
        renderBuiltins();

        const enabled = state.builtins.filter((b) => b.enabled).length
            + state.filters.filter((f) => f.enabled && !f.error).length;
        document.getElementById("review-tab-count-filters").textContent = `(${enabled} on)`;
    }

    // == Tabs ==

    // The section shown is the one in the URL's hash, so that links and the back button work
    const SECTIONS = ["yamls", "filters"];

    function currentSection() {
        const hash = location.hash.slice(1);
        return SECTIONS.includes(hash) ? hash : "yamls";
    }

    function showSection() {
        const section = currentSection();
        for (const name of SECTIONS) {
            document.getElementById(`review-section-${name}`).hidden = name !== section;
        }
        for (const link of root.querySelectorAll(".review-tabs a")) {
            const selected = link.dataset.tab === section;
            link.classList.toggle("selected", selected);
            link.setAttribute("aria-selected", selected);
        }
    }

    window.addEventListener("hashchange", showSection);
    showSection();

    const highlightListeners = new WeakMap();

    // Shows the filter that raised a flag: its row in the filters, and its editor when it's
    // one of the room's
    function goToFilter(flag) {
        if (location.hash !== "#filters") {
            location.hash = "filters";
        }
        showSection();

        const selector = flag.builtin
            ? `tr[data-builtin-id="${CSS.escape(flag.filter_id)}"]`
            : `tr[data-filter-id="${CSS.escape(flag.filter_id)}"]`;
        const row = root.querySelector(selector);
        if (row) {
            row.scrollIntoView({ block: "center" });
            // The listeners of a highlight that's still playing would end this one
            highlightListeners.get(row)?.abort();
            row.classList.remove("review-highlight");
            // Restarts the animation when the same row is highlighted again
            void row.offsetWidth;
            row.classList.add("review-highlight");
            // Left on, the animation would play again whenever the tab is shown again
            const listeners = new AbortController();
            highlightListeners.set(row, listeners);
            const clear = () => {
                row.classList.remove("review-highlight");
                listeners.abort();
            };
            row.addEventListener("animationend", clear, { signal: listeners.signal });
            row.addEventListener("animationcancel", clear, { signal: listeners.signal });
        }

        if (!flag.builtin) {
            const entry = state.filters.find((f) => String(f.id) === flag.filter_id);
            if (entry) {
                openEditor(entry);
            }
        }
    }

    // == Results ==

    const search = document.getElementById("review-search");
    const onlyFlagged = document.getElementById("review-only-flagged");
    const minSeverity = document.getElementById("review-min-severity");
    for (const input of [search, onlyFlagged, minSeverity]) {
        input.addEventListener("input", renderResults);
    }

    function shownFlags(yaml) {
        const min = SEVERITY_RANK[minSeverity.value];
        // A filter that couldn't be evaluated is always shown, it needs fixing
        return yaml.flags.filter((f) => f.outcome === "error" || SEVERITY_RANK[f.severity] >= min);
    }

    function worstRank(flags, yaml) {
        if (yaml.parse_error) {
            return 3;
        }
        return Math.max(-1, ...flags.map((f) => SEVERITY_RANK[f.severity]));
    }

    function renderResults() {
        const body = document.getElementById("review-results-body");
        const query = search.value.trim().toLowerCase();

        const rows = [];
        let flagged = 0;
        for (const yaml of state.yamls) {
            const flags = shownFlags(yaml);
            const isFlagged = flags.length > 0 || yaml.parse_error;
            if (isFlagged) {
                flagged++;
            }
            if (onlyFlagged.checked && !isFlagged) {
                continue;
            }
            const haystack = [yaml.player_name, yaml.owner_name, ...yaml.games].join(" ").toLowerCase();
            if (query && !haystack.includes(query)) {
                continue;
            }
            rows.push({ yaml, flags, rank: worstRank(flags, yaml) });
        }

        rows.sort((a, b) =>
            b.rank - a.rank
            || b.flags.length - a.flags.length
            || a.yaml.player_name.localeCompare(b.yaml.player_name));

        document.getElementById("review-summary").textContent =
            `${flagged} of ${state.yamls.length} YAMLs flagged`;
        document.getElementById("review-tab-count-yamls").textContent =
            flagged ? `(${flagged} flagged)` : "";

        body.replaceChildren();
        if (rows.length === 0) {
            const text = state.yamls.length === 0
                ? "There are no YAMLs in this room yet."
                : "No YAML matches.";
            body.append(el("tr", {}, el("td", { colspan: 5, text })));
            return;
        }

        for (const { yaml, flags } of rows) {
            const flagCell = el("td", { class: "review-flag-list" });
            if (yaml.parse_error) {
                flagCell.append(el("span", {
                    class: "review-flag review-flag-error",
                    title: yaml.parse_error,
                    text: "Invalid YAML",
                }));
            }
            for (const flag of flags) {
                flagCell.append(flagElement(flag));
            }

            body.append(el("tr", { "data-yaml-id": yaml.id },
                el("td", { text: yaml.player_name }),
                el("td", { text: yaml.games.join(", ") }),
                el("td", { text: yaml.owner_name, title: yaml.owner_id }),
                flagCell,
                el("td", { class: "shrink" },
                    el("a", {
                        href: `/room/${roomId}/download/${yaml.id}`,
                        text: "View",
                        onclick: (e) => { e.preventDefault(); showYaml(roomId, yaml.id, false); },
                    })),
            ));
        }
    }

    function flagElement(flag) {
        let text = flag.name;
        if (flag.game) {
            text += ` (${flag.game})`;
        }

        let cls, title;
        if (flag.outcome === "error") {
            cls = "review-flag review-flag-broken";
            title = flag.detail || "The filter couldn't be evaluated on this YAML";
            text = "Broken filter: " + text;
        } else {
            cls = `review-flag review-flag-${flag.severity}`;
            title = [flag.detail, describeFlag(flag)].filter(Boolean).join("\n");
        }
        title = [title, flag.builtin ? "Click to see the filter" : "Click to edit the filter"]
            .filter(Boolean).join("\n\n");

        return el("span", {
            class: cls + " review-flag-link",
            role: "button",
            tabindex: 0,
            title,
            text,
            onclick: () => goToFilter(flag),
            onkeydown: (e) => {
                if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    goToFilter(flag);
                }
            },
        });
    }

    function describeFlag(flag) {
        if (flag.builtin) {
            return state.builtins.find((b) => b.id === flag.filter_id)?.description;
        }
        return state.filters.find((f) => String(f.id) === flag.filter_id)?.filter.description;
    }

    // == Filter lists ==

    function toggle(checked, onchange, label) {
        return el("input", { type: "checkbox", checked, "aria-label": label, onchange });
    }

    function renderCustomFilters() {
        const body = document.getElementById("review-custom-body");
        body.replaceChildren();

        if (state.filters.length === 0) {
            body.append(el("tr", {}, el("td", {
                colspan: 6,
                text: "This room has no filters of its own yet. Make one, or import an export.",
            })));
            return;
        }

        state.filters.forEach((entry, index) => {
            const filter = entry.filter;
            const nameCell = el("td", {},
                el("strong", { text: filter.name ?? "(unnamed)" }),
                filter.description && el("div", { class: "review-filter-description", text: filter.description }),
                entry.error && el("div", {
                    class: "review-filter-invalid",
                    text: "This filter can't be read, so it isn't used: " + entry.error,
                }),
            );

            const actions = el("td", { class: "shrink review-actions" },
                el("button", {
                    title: "Move up", disabled: index === 0,
                    onclick: () => move(entry.id, -1),
                }, el("i", { class: "fa fa-arrow-up" })),
                el("button", {
                    title: "Move down", disabled: index === state.filters.length - 1,
                    onclick: () => move(entry.id, 1),
                }, el("i", { class: "fa fa-arrow-down" })),
                el("button", { text: "Edit", onclick: () => openEditor(entry) }),
                el("a", {
                    class: "button-emulator",
                    href: `${api}/filters/${entry.id}/export`,
                    title: "Download this filter, in the format of the lobby's built-in filters",
                    text: "Export",
                }),
                el("button", {
                    class: "cancel-button",
                    text: "Delete",
                    onclick: () => {
                        if (confirm(`Delete the filter "${filter.name}"?`)) {
                            change(() => request("DELETE", `${api}/filters/${entry.id}`));
                        }
                    },
                }),
            );

            body.append(el("tr", { class: entry.enabled ? null : "review-disabled", "data-filter-id": entry.id },
                el("td", { class: "shrink" }, toggle(entry.enabled, (e) => change(() =>
                    request("PUT", `${api}/filters/${entry.id}/enabled`, { enabled: e.target.checked })),
                    `Use ${filter.name}`)),
                nameCell,
                el("td", { text: filter.game || "Every game" }),
                el("td", { class: "shrink" }, severityChip(filter.severity)),
                el("td", { text: `${formatTime(entry.last_edited_at)} by ${entry.last_edited_by_name}` }),
                actions,
            ));
        });
    }

    // The order is worked out when the change runs, from the filters as they are after the
    // changes before it, not as they were when the button was clicked
    function move(filterId, offset) {
        change(() => {
            const ids = state.filters.map((f) => f.id);
            const index = ids.indexOf(filterId);
            const target = index + offset;
            if (index === -1 || target < 0 || target >= ids.length) {
                return;
            }
            ids.splice(index, 1);
            ids.splice(target, 0, filterId);
            return request("PUT", `${api}/filter-order`, ids);
        });
    }

    function renderBuiltins() {
        const body = document.getElementById("review-builtin-body");
        body.replaceChildren();
        for (const builtin of state.builtins) {
            body.append(el("tr", { class: builtin.enabled ? null : "review-disabled", "data-builtin-id": builtin.id },
                el("td", { class: "shrink" }, toggle(builtin.enabled, (e) => change(() =>
                    request("PUT", `${api}/builtins/${encodeURIComponent(builtin.id)}/enabled`, { enabled: e.target.checked })),
                    `Use ${builtin.name}`)),
                el("td", {}, el("strong", { text: builtin.name })),
                el("td", { text: builtin.description }),
            ));
        }
    }

    // == Import ==

    const importFile = document.getElementById("review-import-file");
    document.getElementById("review-import").addEventListener("click", () => importFile.click());
    importFile.addEventListener("change", async () => {
        const file = importFile.files[0];
        importFile.value = "";
        if (!file) {
            return;
        }
        const text = await file.text();
        await change(async () => {
            const added = await request("POST", `${api}/import`, text);
            notify(`Imported ${added.length} filter${added.length === 1 ? "" : "s"}`, "confirm");
        });
    });

    // == Editor ==

    const editor = document.getElementById("review-editor");
    const editorError = document.getElementById("review-editor-error");
    const fields = {
        name: document.getElementById("rf-name"),
        description: document.getElementById("rf-description"),
        game: document.getElementById("rf-game"),
        severity: document.getElementById("rf-severity"),
        whenEnabled: document.getElementById("rf-when-enabled"),
        json: document.getElementById("rf-json"),
    };
    // The filter being edited, and the ID of the room's filter it is, if it's one already
    let model = null;
    let editingId = null;
    let tab = "builder";
    // The "only for YAMLs where" condition while its box is unticked, to have it back when
    // the box is ticked again. It isn't saved.
    let setAsideWhen = null;

    function newCheck() {
        return { type: "check", path: "", check: { type: "truthy" } };
    }

    function openEditor(entry) {
        editingId = entry ? entry.id : null;
        const source = entry ? entry.filter : {
            name: "", description: "", game: null, when: null, then: newCheck(), severity: "warning",
        };
        model = structuredClone(source);
        setAsideWhen = null;
        model.then ??= newCheck();
        model.when ??= null;

        document.getElementById("review-editor-title").textContent = entry ? "Edit filter" : "New filter";
        const games = [...new Set(state.yamls.flatMap((y) => y.games))].sort();
        document.getElementById("rf-games").replaceChildren(...games.map((g) => el("option", { value: g })));

        hideEditorError();
        tab = "builder";
        try {
            if (entry?.error) {
                throw new Error(entry.error);
            }
            loadFieldsFromModel();
        } catch (e) {
            // A filter the builder doesn't know the shape of can still be fixed as JSON
            tab = "json";
            fields.json.value = JSON.stringify(source, null, 2);
            showEditorError("The builder can't show this filter, fix it here: " + e.message);
        }
        showTab();
        editor.showModal();
        (tab === "builder" ? fields.name : fields.json).focus();
    }

    function loadFieldsFromModel() {
        fields.name.value = model.name ?? "";
        fields.description.value = model.description ?? "";
        fields.game.value = model.game ?? "";
        fields.severity.value = model.severity ?? "warning";
        fields.whenEnabled.checked = model.when !== null;
        renderPredicates();
    }

    function readFieldsIntoModel() {
        model.name = fields.name.value.trim();
        const description = fields.description.value.trim();
        model.description = description || undefined;
        model.game = fields.game.value.trim() || null;
        model.severity = fields.severity.value;
    }

    fields.whenEnabled.addEventListener("change", () => {
        if (fields.whenEnabled.checked) {
            model.when = setAsideWhen ?? newCheck();
            setAsideWhen = null;
        } else {
            setAsideWhen = model.when;
            model.when = null;
        }
        renderPredicates();
    });

    function renderPredicates() {
        document.getElementById("rf-then").replaceChildren(
            predicateEditor(model.then, (p) => { model.then = p; }, null));
        const when = document.getElementById("rf-when");
        when.replaceChildren(model.when === null ? "" :
            predicateEditor(model.when, (p) => { model.when = p; }, null));
    }

    // The editor of a condition. `set` replaces it in its parent, `remove` takes it out of the
    // group it is in, when it is in one.
    function predicateEditor(pred, set, remove) {
        const typeSelect = el("select", { class: "rf-pred-type" },
            GROUPS.map((g) => el("option", { value: g.type, text: g.label, selected: g.type === pred.type })));
        typeSelect.addEventListener("change", () => {
            set(convertPredicate(pred, typeSelect.value));
            renderPredicates();
        });

        const header = el("div", { class: "rf-pred-header" }, typeSelect);
        const box = el("div", { class: `rf-pred rf-pred-${pred.type}` }, header);

        if (pred.type === "check") {
            header.append(...checkFields(pred));
        } else if (pred.type === "not") {
            box.append(el("div", { class: "rf-pred-children" },
                predicateEditor(pred.predicate, (p) => { pred.predicate = p; }, null)));
        } else {
            if (pred.type === "at_least") {
                const clampCount = (value) => Math.min(Math.max(toInteger(value), 1), pred.predicates.length);
                pred.count = clampCount(pred.count);
                header.append(
                    numberInput(pred.count, clampCount, (n) => { pred.count = n; }, { min: 1, max: pred.predicates.length }),
                    el("span", { text: "of" }));
            }
            const children = el("div", { class: "rf-pred-children" });
            // A group keeps at least one condition: an empty "all of" would match every YAML
            const removable = pred.predicates.length > 1;
            pred.predicates.forEach((child, i) => {
                children.append(predicateEditor(child,
                    (p) => { pred.predicates[i] = p; },
                    removable ? () => {
                        pred.predicates.splice(i, 1);
                        if (pred.type === "at_least") {
                            pred.count = Math.min(pred.count, pred.predicates.length);
                        }
                        renderPredicates();
                    } : null));
            });
            children.append(el("div", { class: "rf-pred-add" },
                el("button", {
                    type: "button", text: "Add an option",
                    onclick: () => { pred.predicates.push(newCheck()); renderPredicates(); },
                }),
                el("button", {
                    type: "button", text: "Add a group",
                    onclick: () => { pred.predicates.push({ type: "or", predicates: [newCheck()] }); renderPredicates(); },
                }),
            ));
            box.append(children);
        }

        if (remove) {
            header.append(el("button", {
                type: "button", class: "rf-remove", title: "Remove", onclick: remove,
            }, el("i", { class: "fa fa-xmark" })));
        }
        return box;
    }

    // Turns a condition into another type of condition, keeping what can be kept
    function convertPredicate(pred, type) {
        const children = pred.predicates ?? (pred.type === "not" ? [pred.predicate] : [pred]);
        switch (type) {
            case "check":
                return pred.type === "check" ? pred : (children.find((c) => c.type === "check") ?? newCheck());
            case "not":
                return { type: "not", predicate: pred.type === "not" ? pred.predicate : pred };
            case "at_least":
                return { type: "at_least", count: 1, predicates: children };
            default:
                return { type, predicates: children };
        }
    }

    function checkFields(pred) {
        const path = el("input", { type: "text", value: pred.path, placeholder: "option", class: "rf-path" });
        path.addEventListener("input", () => { pred.path = path.value.trim(); });

        const typeSelect = el("select", {},
            CHECKS.map((c) => el("option", { value: c.type, text: c.label, selected: c.type === pred.check.type })));
        const values = el("span", { class: "rf-values" });
        const renderValues = () => values.replaceChildren(...valueFields(pred.check));
        typeSelect.addEventListener("change", () => {
            pred.check = convertCheck(pred.check, typeSelect.value);
            renderValues();
        });
        renderValues();

        return [path, typeSelect, values];
    }

    function convertCheck(check, type) {
        const spec = CHECKS.find((c) => c.type === type);
        const old = CHECKS.find((c) => c.type === check.type);
        if (spec.value === "text" && old.value === "text") {
            return { type, value: check.value };
        }
        if (spec.value === "number" && old.value === "number") {
            return { type, value: check.value };
        }
        switch (spec.value) {
            case "text": return { type, value: "" };
            case "number": return { type, value: 0 };
            case "range": return { type, min: 0, max: 0 };
            case "pattern": return { type, pattern: "" };
            case "count": return { type, check: { type: "greater_than", value: 0 } };
            default: return { type };
        }
    }

    // The inputs of what a check compares with
    function valueFields(check) {
        normalizeCheck(check);
        const spec = CHECKS.find((c) => c.type === check.type);
        const textInput = (value, onvalue) => {
            const elmt = el("input", { type: "text", value, class: "rf-value" });
            elmt.addEventListener("input", () => onvalue(elmt.value));
            return elmt;
        };

        switch (spec.value) {
            case "text":
                return [textInput(check.value, (v) => { check.value = v; })];
            case "number":
                return [numberInput(check.value, toInteger, (n) => { check.value = n; })];
            case "range":
                return [
                    numberInput(check.min, toInteger, (n) => { check.min = n; }),
                    el("span", { text: "and" }),
                    numberInput(check.max, toInteger, (n) => { check.max = n; }),
                ];
            case "pattern":
                return [textInput(check.pattern, (v) => { check.pattern = v; })];
            case "count": {
                const inner = check.check;
                const select = el("select", {},
                    COUNT_CHECKS.map((t) => el("option", {
                        value: t, text: CHECKS.find((c) => c.type === t).label, selected: t === inner.type,
                    })));
                const innerValues = el("span", { class: "rf-values" });
                const renderInner = () => innerValues.replaceChildren(...countValueFields(check.check));
                select.addEventListener("change", () => {
                    check.check = convertCheck(check.check, select.value);
                    renderInner();
                });
                renderInner();
                return [select, innerValues];
            }
            default:
                return [];
        }
    }

    // The number of entries is compared with numbers, but `equals` keeps its value as text
    function countValueFields(check) {
        if (check.type === "equals" || check.type === "not_equals") {
            check.value = String(toInteger(check.value));
            return [numberInput(check.value, toInteger, (n) => { check.value = String(n); }, { min: 0 })];
        }
        return valueFields(check);
    }

    // The server keeps numbers as 64-bit integers, and JavaScript can only be exact up to
    // 2^53, so they're kept within that
    function toInteger(value) {
        const n = Math.trunc(Number(value));
        if (!Number.isFinite(n)) {
            return 0;
        }
        return Math.min(Math.max(n, Number.MIN_SAFE_INTEGER), Number.MAX_SAFE_INTEGER);
    }

    // A number input that keeps the model a valid integer as it's typed in, and shows what
    // was kept once it's left (1.5 becomes 1)
    function numberInput(value, normalize, onvalue, attrs = {}) {
        const elmt = el("input", { type: "number", step: 1, value: String(value), class: "rf-small", ...attrs });
        elmt.addEventListener("input", () => onvalue(normalize(elmt.value)));
        elmt.addEventListener("change", () => { elmt.value = String(normalize(elmt.value)); });
        return elmt;
    }

    // Makes the values of a check the types the server takes, for checks written as JSON:
    // text for equals and contains, integers for comparisons, a numeric check for count
    function normalizeCheck(check) {
        const spec = CHECKS.find((c) => c.type === check.type);
        switch (spec.value) {
            case "text":
                check.value = check.value === undefined || check.value === null ? "" : String(check.value);
                break;
            case "number":
                check.value = toInteger(check.value);
                break;
            case "range":
                check.min = toInteger(check.min);
                check.max = toInteger(check.max);
                break;
            case "pattern":
                check.pattern = check.pattern === undefined || check.pattern === null ? "" : String(check.pattern);
                break;
            case "count":
                if (typeof check.check !== "object" || check.check === null || !COUNT_CHECKS.includes(check.check.type)) {
                    check.check = { type: "greater_than", value: 0 };
                }
                break;
        }
    }

    // The filter as the API takes it
    function filterFromModel() {
        readFieldsIntoModel();
        const filter = {
            name: model.name,
            game: model.game,
            when: model.when,
            then: model.then,
            severity: model.severity,
        };
        if (model.description) {
            filter.description = model.description;
        }
        return filter;
    }

    // Switches between the builder and the JSON, carrying the filter over
    function setTab(next) {
        if (next === "json" && tab === "builder") {
            fields.json.value = JSON.stringify(filterFromModel(), null, 2);
        }
        if (next === "builder" && tab === "json") {
            let parsed;
            try {
                parsed = JSON.parse(fields.json.value);
            } catch (e) {
                showEditorError("The JSON can't be read: " + e.message);
                return;
            }
            const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
            if (!isObject(parsed) || !isObject(parsed.then)) {
                showEditorError("The JSON should be one filter, with a \"then\" condition");
                return;
            }
            const previous = model;
            model = parsed;
            model.when ??= null;
            setAsideWhen = null;
            try {
                loadFieldsFromModel();
            } catch (e) {
                // The JSON stays what's edited, and saved
                model = previous;
                showEditorError("The builder can't show this filter, keep editing its JSON: " + e.message);
                return;
            }
        }
        hideEditorError();
        tab = next;
        showTab();
    }

    function showTab() {
        document.getElementById("review-editor-builder").hidden = tab !== "builder";
        document.getElementById("review-editor-json").hidden = tab !== "json";
        for (const button of editor.querySelectorAll(".review-editor-tabs button")) {
            button.classList.toggle("selected", button.dataset.tab === tab);
        }
    }

    for (const button of editor.querySelectorAll(".review-editor-tabs button")) {
        button.addEventListener("click", () => setTab(button.dataset.tab));
    }

    function showEditorError(message) {
        editorError.textContent = message;
        editorError.hidden = false;
    }

    function hideEditorError() {
        editorError.hidden = true;
    }

    document.getElementById("rf-cancel").addEventListener("click", () => editor.close());
    const saveButton = document.getElementById("rf-save");
    saveButton.addEventListener("click", async () => {
        // A second click while the first is on its way would make the filter twice
        if (saveButton.disabled) {
            return;
        }
        saveButton.disabled = true;
        try {
            const body = tab === "json" ? fields.json.value : JSON.stringify(filterFromModel());
            if (editingId === null) {
                await request("POST", `${api}/filters`, body);
            } else {
                await request("PUT", `${api}/filters/${editingId}`, body);
            }
        } catch (e) {
            showEditorError(e.message);
            return;
        } finally {
            saveButton.disabled = false;
        }
        editor.close();
        await load();
    });

    document.getElementById("review-new-filter").addEventListener("click", () => openEditor(null));

    load();
})();
