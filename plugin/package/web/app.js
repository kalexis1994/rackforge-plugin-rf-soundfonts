(() => {
  "use strict";

  const PROTOCOL = "rackforge.plugin.web@1";
  const surface = document.body.dataset.surface;
  const root = document.getElementById("plugin-root");
  const hostOrigin = window.location.origin;
  let context = null;
  let sequence = 0;
  let activeCollection = "dls";
  let openLibrary = null;
  let searchQuery = "";
  let configSearchQuery = "";
  let activeDraftId = null;
  let editMode = false;
  let localProgramName = null;
  let discardPrompt = false;
  let bridgeError = "";
  let activeEditorPage = "layer-a";
  let focusProgramName = false;
  const localFieldValues = new Map();
  const previewTimers = new Map();
  const editorVisuals = new Map();

  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = text;
    return element;
  }

  function svgNode(tag, attributes = {}) {
    const element = document.createElementNS("http://www.w3.org/2000/svg", tag);
    Object.entries(attributes).forEach(([name, value]) =>
      element.setAttribute(name, String(value)),
    );
    return element;
  }

  function visualField(page, suffix) {
    return (page.fields ?? []).find((field) => field.id.endsWith(suffix));
  }

  function visualNumber(field, fallback) {
    if (!field) return { value: fallback, inherited: true, label: "N/A" };
    const value = displayedFieldValue(field);
    if (value.type === "inherited") {
      return { value: fallback, inherited: true, label: "INHERITED" };
    }
    if (value.type !== "integer") {
      return { value: fallback, inherited: true, label: "N/A" };
    }
    const decimals = field.kind.decimals ?? 0;
    return {
      value: value.value / 10 ** decimals,
      inherited: false,
      label: valueText(value, field.kind),
    };
  }

  function registerVisual(page, element, update) {
    editorVisuals.set(page.id, {
      fieldIds: (page.fields ?? []).map((field) => field.id),
      update,
    });
    update();
    return element;
  }

  function updateFieldVisual(fieldId) {
    editorVisuals.forEach((visual) => {
      if (visual.fieldIds.includes(fieldId)) visual.update();
    });
  }

  function renderEnvelopeVisual(page) {
    const panel = node("div", "parameter-visual envelope-visual");
    const header = node("div", "visual-header");
    header.append(
      node("div", "visual-title", page.id.includes("pitch") ? "PITCH ENVELOPE" : "AMPLITUDE ENVELOPE"),
      node("span", "visual-live", "LIVE PREVIEW"),
    );
    const svg = svgNode("svg", {
      viewBox: "0 0 720 230",
      role: "img",
      "aria-label": `${page.label} envelope graph`,
      preserveAspectRatio: "none",
    });
    const values = node("div", "visual-values");
    panel.append(header, svg, values);

    const draw = () => {
      const attack = visualNumber(visualField(page, ".attack"), 0.12);
      const decay = visualNumber(visualField(page, ".decay"), 0.35);
      const sustain = visualNumber(visualField(page, ".sustain"), 0.72);
      const release = visualNumber(visualField(page, ".release"), 0.55);
      const phaseWidth = (seconds) => 34 + Math.log1p(Math.max(0, seconds)) / Math.log(61) * 118;
      let attackWidth = phaseWidth(attack.value);
      let decayWidth = phaseWidth(decay.value);
      let releaseWidth = phaseWidth(release.value);
      const available = 570;
      const total = attackWidth + decayWidth + releaseWidth;
      if (total > available) {
        const scale = available / total;
        attackWidth *= scale;
        decayWidth *= scale;
        releaseWidth *= scale;
      }
      const left = 44;
      const top = 28;
      const bottom = 188;
      const attackX = left + attackWidth;
      const decayX = attackX + decayWidth;
      const releaseX = 676;
      const sustainX = releaseX - releaseWidth;
      const sustainY = top + (1 - Math.max(0, Math.min(1, sustain.value))) * (bottom - top);
      svg.replaceChildren();
      [0, 0.25, 0.5, 0.75, 1].forEach((step) => {
        svg.append(
          svgNode("line", {
            x1: left,
            y1: top + step * (bottom - top),
            x2: releaseX,
            y2: top + step * (bottom - top),
            class: "visual-grid-line",
          }),
        );
      });
      const phases = [
        [left, bottom, attackX, top, attack.inherited],
        [attackX, top, decayX, sustainY, decay.inherited],
        [decayX, sustainY, sustainX, sustainY, sustain.inherited],
        [sustainX, sustainY, releaseX, bottom, release.inherited],
      ];
      phases.forEach(([x1, y1, x2, y2, inherited]) =>
        svg.append(
          svgNode("line", {
            x1,
            y1,
            x2,
            y2,
            class: `envelope-phase${inherited ? " inherited" : ""}`,
          }),
        ),
      );
      [
        [left, bottom],
        [attackX, top],
        [decayX, sustainY],
        [sustainX, sustainY],
        [releaseX, bottom],
      ].forEach(([cx, cy]) =>
        svg.append(svgNode("circle", { cx, cy, r: 6, class: "envelope-point" })),
      );
      values.replaceChildren();
      [
        ["A", attack],
        ["D", decay],
        ["S", sustain],
        ["R", release],
      ].forEach(([label, item]) => {
        const chip = node("div", `visual-value${item.inherited ? " inherited" : ""}`);
        chip.append(node("span", "", label), node("strong", "", item.label));
        values.append(chip);
      });
    };
    return registerVisual(page, panel, draw);
  }

  function renderLfoVisual(page) {
    const panel = node("div", "parameter-visual lfo-visual");
    const header = node("div", "visual-header");
    header.append(node("div", "visual-title", "MODULATION SHAPE"), node("span", "visual-live", "LFO"));
    const svg = svgNode("svg", {
      viewBox: "0 0 720 190",
      role: "img",
      "aria-label": "LFO modulation graph",
      preserveAspectRatio: "none",
    });
    const values = node("div", "visual-values compact");
    panel.append(header, svg, values);
    const draw = () => {
      const rate = visualNumber(visualField(page, ".frequency"), 5);
      const delay = visualNumber(visualField(page, ".delay"), 0);
      const pitch = visualNumber(visualField(page, ".pitch"), 0);
      const modPitch = visualNumber(visualField(page, ".mod-pitch"), 50);
      const left = 44;
      const right = 676;
      const center = 94;
      const delayX = left + Math.min(0.38, Math.max(0, delay.value) / 12) * (right - left);
      const cycles = 1.5 + Math.min(7.5, Math.max(0.01, rate.value) * 0.8);
      const amplitude = 28 + Math.min(48, Math.max(Math.abs(pitch.value), Math.abs(modPitch.value)) / 50);
      let path = `M ${left} ${center} L ${delayX} ${center}`;
      const samples = 120;
      for (let index = 0; index <= samples; index += 1) {
        const progress = index / samples;
        const x = delayX + progress * (right - delayX);
        const y = center - Math.sin(progress * cycles * Math.PI * 2) * amplitude;
        path += ` L ${x.toFixed(2)} ${y.toFixed(2)}`;
      }
      svg.replaceChildren(
        svgNode("line", { x1: left, y1: center, x2: right, y2: center, class: "visual-axis" }),
        svgNode("line", { x1: delayX, y1: 20, x2: delayX, y2: 168, class: "delay-marker" }),
        svgNode("path", {
          d: path,
          class: `lfo-wave${rate.inherited ? " inherited" : ""}`,
          fill: "none",
        }),
      );
      values.replaceChildren();
      [["RATE", rate], ["DELAY", delay], ["PITCH", pitch], ["WHEEL", modPitch]].forEach(([label, item]) => {
        const chip = node("div", `visual-value${item.inherited ? " inherited" : ""}`);
        chip.append(node("span", "", label), node("strong", "", item.label));
        values.append(chip);
      });
    };
    return registerVisual(page, panel, draw);
  }

  function renderRangeVisual(page) {
    const panel = node("div", "parameter-visual range-visual");
    const header = node("div", "visual-header");
    header.append(node("div", "visual-title", "PERFORMANCE ZONES"), node("span", "visual-live", "MIDI 0–127"));
    const lanes = node("div", "range-lanes");
    panel.append(header, lanes);
    const draw = () => {
      const keyLow = visualNumber(visualField(page, ".key-low"), 0);
      const keyHigh = visualNumber(visualField(page, ".key-high"), 127);
      const velLow = visualNumber(visualField(page, ".vel-low"), 0);
      const velHigh = visualNumber(visualField(page, ".vel-high"), 127);
      lanes.replaceChildren();
      [["KEY", keyLow, keyHigh], ["VELOCITY", velLow, velHigh]].forEach(([label, low, high]) => {
        const row = node("div", "range-lane");
        const copy = node("div", "range-lane-copy");
        copy.append(node("span", "", label), node("strong", "", `${Math.round(low.value)} – ${Math.round(high.value)}`));
        const track = node("div", "range-lane-track");
        const fill = node("i");
        fill.style.left = `${Math.max(0, Math.min(100, low.value / 127 * 100))}%`;
        fill.style.right = `${Math.max(0, Math.min(100, (127 - high.value) / 127 * 100))}%`;
        track.append(fill);
        row.append(copy, track);
        lanes.append(row);
      });
    };
    return registerVisual(page, panel, draw);
  }

  function request(method, params) {
    sequence += 1;
    window.parent.postMessage(
      {
        protocol: PROTOCOL,
        kind: "request",
        request_id: `rf-soundfonts-${sequence}`,
        method,
        params,
      },
      hostOrigin,
    );
  }

  function sameValue(left, right) {
    return JSON.stringify(left) === JSON.stringify(right);
  }

  function displayedFieldValue(field) {
    const local = localFieldValues.get(field.id);
    if (local && sameValue(local, field.value)) {
      localFieldValues.delete(field.id);
      return field.value;
    }
    return local ?? field.value;
  }

  function editField(draft, field, value, preview) {
    localFieldValues.set(field.id, value);
    updateFieldVisual(field.id);
    request("plugin.edit_program_field", {
      draft_id: draft.draft_id,
      field_id: field.id,
      value,
      preview,
    });
  }

  function previewField(draft, field, value) {
    localFieldValues.set(field.id, value);
    updateFieldVisual(field.id);
    const existing = previewTimers.get(field.id);
    if (existing) window.clearTimeout(existing);
    previewTimers.set(
      field.id,
      window.setTimeout(() => {
        previewTimers.delete(field.id);
        request("plugin.edit_program_field", {
          draft_id: draft.draft_id,
          field_id: field.id,
          value,
          preview: true,
        });
      }, 70),
    );
  }

  function commitField(draft, field, value) {
    const existing = previewTimers.get(field.id);
    if (existing) window.clearTimeout(existing);
    previewTimers.delete(field.id);
    editField(draft, field, value, false);
  }

  function selectedSound(instance) {
    return instance.sounds.find(
      (candidate) => candidate.id === instance.selected_sound_id,
    );
  }

  function collectionFor(sound) {
    return sound?.bank?.toLowerCase() === "custom" ? "custom" : "dls";
  }

  function pluginHeader(instance, label) {
    const header = node("header", "plugin-header");
    const copy = node("div");
    copy.append(
      node("span", "eyebrow", label),
      node("h1", "", instance.plugin_name),
      node(
        "p",
        "",
        summariseInstall(instance),
      ),
    );
    const badge = node("span", "api-badge", "WEB API 1");
    header.append(copy, badge);
    return header;
  }

  /// Lowest and highest key of an eighty-eight note keyboard.
  const KEYBOARD_LOW = 21;
  const KEYBOARD_HIGH = 108;
  const BLACK_KEYS = [1, 3, 6, 8, 10];
  const NOTE_NAMES = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
  ];

  function noteName(note) {
    return `${NOTE_NAMES[note % 12]}${Math.floor(note / 12) - 1}`;
  }

  /// Reads the marks the plugin attached to a sound.
  ///
  /// Only this plugin writes them and only this surface reads them, so the
  /// shape is ours. Anything missing comes back null rather than zero: an
  /// instrument that reported nothing is not an instrument with no keys, and a
  /// cover drawn from invented numbers would look confidently wrong.
  function factsOf(sound) {
    const marks = sound.tags ?? [];
    const value = (name) => {
      const hit = marks.find((mark) => mark.startsWith(`${name}:`));
      return hit ? hit.slice(name.length + 1) : null;
    };
    const span = (name) => {
      const raw = value(name);
      if (!raw) return null;
      const [low, high] = raw.split("-").map(Number);
      return Number.isFinite(low) && Number.isFinite(high) ? [low, high] : null;
    };
    const count = (name) => {
      const raw = Number(value(name));
      return Number.isFinite(raw) ? raw : null;
    };
    return {
      keys: span("keys"),
      roots: span("roots"),
      zones: count("zones"),
      samples: count("samples"),
      layers: count("layers"),
      bytes: count("bytes"),
      looping: marks.includes("looping"),
    };
  }

  /// Draws an instrument as the keyboard it covers.
  ///
  /// Two ranges, because they say different things. The pale one is every key
  /// that answers, which for a converted library is usually the whole
  /// keyboard. The bright one is where the recordings actually are, and that
  /// is what tells one trumpet from another. Velocity layers stack above it,
  /// so an instrument with five ways to strike a note looks unlike one with a
  /// single sample per key.
  function coverArt(facts) {
    const W = 320;
    const H = 116;
    const PAD = 10;
    const span = W - PAD * 2;
    const at = (note) =>
      PAD + ((note - KEYBOARD_LOW) / (KEYBOARD_HIGH - KEYBOARD_LOW)) * span;
    const width = span / (KEYBOARD_HIGH - KEYBOARD_LOW + 1);

    const reach = facts.keys ?? [KEYBOARD_LOW, KEYBOARD_HIGH];
    const roots = facts.roots ?? reach;
    let keys = "";
    for (let note = KEYBOARD_LOW; note <= KEYBOARD_HIGH; note += 1) {
      const black = BLACK_KEYS.includes(note % 12);
      const recorded = note >= roots[0] && note <= roots[1];
      const reachable = note >= reach[0] && note <= reach[1];
      keys += `<rect x="${at(note).toFixed(1)}" y="${black ? H - 46 : H - 34}"
        width="${(width * 0.78).toFixed(2)}" height="${black ? 20 : 26}" rx="1"
        class="${recorded ? "cover-recorded" : reachable ? "cover-reachable" : "cover-mute"}"/>`;
    }

    let bands = "";
    const layers = Math.min(facts.layers ?? 1, 8);
    for (let layer = 0; layer < layers; layer += 1) {
      const height = 6;
      const gap = 3;
      bands += `<rect x="${at(roots[0]).toFixed(1)}"
        y="${H - 56 - (layer + 1) * (height + gap)}"
        width="${Math.max(width, at(roots[1]) - at(roots[0])).toFixed(1)}"
        height="${height}" rx="2" class="cover-layer"
        opacity="${(0.30 + layer * 0.14).toFixed(2)}"/>`;
    }

    const holder = node("div", "cover");
    holder.innerHTML = `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none"
      role="img" aria-hidden="true">${bands}${keys}</svg>`;
    return holder;
  }

  function memoryText(bytes) {
    if (bytes === null) return "";
    return bytes >= 1048576
      ? `${Math.round(bytes / 1048576)} MiB`
      : `${Math.round(bytes / 1024)} KiB`;
  }

  /// Groups the sounds by the bank they belong to, in the plugin's own order.
  ///
  /// Banks arrive as a list with their names because an identifier cannot
  /// carry one: `Acordeon Hohner Corona II` becomes
  /// `acordeon-hohner-corona-ii` and no amount of tidying brings the capitals
  /// back. A sound whose bank was never declared still gets a home rather than
  /// vanishing from the browser.
  function librariesOf(instance) {
    const declared = new Map();
    (instance.banks ?? []).forEach((bank) => {
      declared.set(bank.id, { id: bank.id, name: bank.name, order: bank.order ?? 0, sounds: [] });
    });
    instance.sounds.forEach((sound) => {
      const id = sound.bank ?? "";
      if (!declared.has(id)) {
        declared.set(id, { id, name: id || "Sin librería", order: 9999, sounds: [] });
      }
      declared.get(id).sounds.push(sound);
    });
    return [...declared.values()]
      .filter((library) => library.sounds.length > 0)
      .sort((left, right) => left.order - right.order);
  }

  /// One line saying what is installed, for the surface header.
  function summariseInstall(instance) {
    const libraries = (instance.banks ?? []).length;
    const total = instance.sounds.length;
    const bytes = instance.sounds.reduce(
      (sum, sound) => sum + (factsOf(sound).bytes ?? 0),
      0,
    );
    const parts = [`${total} instrumento${total === 1 ? "" : "s"}`];
    if (libraries > 1) {
      parts.push(`${libraries} librerías`);
    }
    if (bytes > 0) {
      parts.push(memoryText(bytes));
    }
    return parts.join(" · ");
  }

  function renderPlay(instance) {
    root.replaceChildren();
    root.append(pluginHeader(instance, "PLAY SURFACE"));

    const libraries = librariesOf(instance);
    const current = selectedSound(instance);
    const total = instance.sounds.length;

    root.append(currentCard(instance, current, libraries));

    const browser = node("section", "library-browser");
    const toolbar = node("div", "toolbar");
    const search = node("input");
    search.type = "search";
    search.value = searchQuery;
    search.placeholder = "Buscar instrumento";
    search.setAttribute("aria-label", "Buscar instrumento");
    const count = node("span", "count");
    toolbar.append(search, count);
    const body = node("div", "browser-body");
    browser.append(toolbar, body);
    root.append(browser);

    function draw() {
      searchQuery = search.value;
      const query = searchQuery.trim().toLowerCase();
      body.replaceChildren();

      // A search reaches across every library at once. Having to pick the
      // right shelf before being allowed to look is the thing a search is for
      // avoiding, and on stage it is the difference between finding a sound
      // and giving up on it.
      if (query) {
        const hits = instance.sounds.filter(
          (sound) =>
            sound.name.toLowerCase().includes(query) ||
            (sound.detail ?? "").toLowerCase().includes(query),
        );
        count.textContent = `${hits.length} de ${total}`;
        if (hits.length === 0) {
          body.append(node("p", "empty-collection", "Ningún instrumento coincide."));
          return;
        }
        const named = new Map(libraries.map((library) => [library.id, library.name]));
        body.append(instrumentGrid(instance, hits, named));
        return;
      }

      const open = libraries.find((library) => library.id === openLibrary);
      if (open) {
        count.textContent = `${open.sounds.length} instrumento${open.sounds.length === 1 ? "" : "s"}`;
        const back = node("button", "crumb");
        back.type = "button";
        back.append(node("span", "crumb-arrow", "←"), node("strong", "", open.name));
        back.addEventListener("click", () => {
          openLibrary = null;
          draw();
        });
        body.append(back, instrumentGrid(instance, open.sounds, new Map()));
        return;
      }

      count.textContent = `${libraries.length} librerías · ${total} instrumentos`;
      const grid = node("div", "library-grid");
      libraries.forEach((library) => grid.append(libraryCard(instance, library)));
      body.append(grid);
    }

    search.addEventListener("input", draw);
    draw();
  }

  /// The card naming what is loaded and ready to play.
  function currentCard(instance, current, libraries) {
    const card = node("section", "current-program");
    const copy = node("div");
    const library = libraries.find((entry) => entry.id === current?.bank);
    copy.append(
      node("span", "eyebrow", library ? library.name : "SIN INSTRUMENTO"),
      node("h2", "", current?.name ?? "Ningún instrumento cargado"),
      node("p", "", current?.detail ?? ""),
    );
    card.append(copy, node("span", "pulse", "MIDI READY"));
    return card;
  }

  /// One library, shown as the instruments it holds.
  function libraryCard(instance, library) {
    const card = node("button", "library-card");
    card.type = "button";
    // The library's cover is its widest instrument, which is the one that
    // best describes what the folder is for.
    const widest = library.sounds.reduce((best, sound) => {
      const facts = factsOf(sound);
      const reach = facts.roots ? facts.roots[1] - facts.roots[0] : -1;
      return reach > best.reach ? { sound, reach } : best;
    }, { sound: library.sounds[0], reach: -1 });
    const holds = library.sounds.some((sound) => factsOf(sound).roots);
    if (holds) {
      card.append(coverArt(factsOf(widest.sound)));
    } else {
      card.append(node("div", "cover cover-empty"));
    }
    const meta = node("div", "card-meta");
    const bytes = library.sounds.reduce(
      (sum, sound) => sum + (factsOf(sound).bytes ?? 0),
      0,
    );
    meta.append(
      node("strong", "", library.name),
      node(
        "small",
        "",
        `${library.sounds.length} instrumento${library.sounds.length === 1 ? "" : "s"}${bytes ? ` · ${memoryText(bytes)}` : ""}`,
      ),
    );
    card.append(meta);
    if (library.sounds.some((sound) => sound.id === instance.selected_sound_id)) {
      card.classList.add("holds-selected");
    }
    card.addEventListener("click", () => {
      openLibrary = library.id;
      renderPlay(instance);
    });
    return card;
  }

  /// The instruments of one library, or the hits of a search.
  function instrumentGrid(instance, sounds, libraryNames) {
    const grid = node("div", "instrument-grid");
    sounds.forEach((sound) => {
      const facts = factsOf(sound);
      const card = node(
        "button",
        `instrument-card${sound.id === instance.selected_sound_id ? " selected" : ""}`,
      );
      card.type = "button";
      if (facts.roots) {
        card.append(coverArt(facts));
      } else {
        card.append(node("div", "cover cover-empty"));
      }
      const meta = node("div", "card-meta");
      const origin = libraryNames.get(sound.bank ?? "");
      meta.append(node("strong", "", sound.name));
      if (origin) {
        meta.append(node("small", "origin", origin));
      }
      if (facts.roots) {
        const range = `${noteName(facts.roots[0])}–${noteName(facts.roots[1])}`;
        const parts = [range, `${facts.zones ?? 0} zonas`];
        if ((facts.layers ?? 1) > 1) {
          parts.push(`${facts.layers} capas`);
        }
        meta.append(node("small", "", parts.join(" · ")));
      } else if (sound.detail) {
        meta.append(node("small", "", sound.detail));
      }
      card.append(meta);
      card.append(
        node(
          "span",
          "instrument-status",
          sound.id === instance.selected_sound_id ? "SONANDO" : "CARGAR",
        ),
      );
      card.addEventListener("click", () =>
        request("plugin.select_sound", { sound_id: sound.id }),
      );
      grid.append(card);
    });
    return grid;
  }


  function renderConfigLibrary(instance) {
    root.replaceChildren();
    root.append(pluginHeader(instance, "CONFIG SURFACE"));

    const customPrograms = instance.sounds.filter(
      (sound) => collectionFor(sound) === "custom",
    );
    const heading = node("section", "config-library-heading");
    const headingCopy = node("div");
    headingCopy.append(
      node("span", "eyebrow", "CUSTOM PROGRAMS"),
      node("h2", "", "Program editor"),
      node(
        "p",
        "",
        "Create or edit a custom program using the same configuration model as the hardware display.",
      ),
    );
    const addButton = node("button", "editor-primary", "ADD NEW");
    addButton.type = "button";
    addButton.addEventListener("click", () =>
      request("plugin.begin_program_edit", { program_id: null }),
    );
    heading.append(headingCopy, addButton);
    root.append(heading);

    const browser = node("section", "program-browser config-browser");
    const toolbar = node("div", "toolbar");
    const search = node("input");
    search.type = "search";
    search.value = configSearchQuery;
    search.placeholder = "Search custom programs";
    search.setAttribute("aria-label", "Search custom programs");
    const count = node("span", "count", `${customPrograms.length} CUSTOM`);
    toolbar.append(search, count);
    const list = node("div", "program-list");
    browser.append(toolbar, list);
    root.append(browser);

    function drawList() {
      configSearchQuery = search.value;
      const query = configSearchQuery.trim().toLowerCase();
      const filtered = customPrograms.filter(
        (sound) =>
          !query ||
          sound.name.toLowerCase().includes(query) ||
          (sound.detail ?? "").toLowerCase().includes(query),
      );
      count.textContent = `${filtered.length} CUSTOM`;
      list.replaceChildren();
      if (filtered.length === 0) {
        list.append(
          node(
            "p",
            "empty-collection",
            customPrograms.length
              ? "No custom programs match this search."
              : "No custom programs yet. Use ADD NEW to create one.",
          ),
        );
        return;
      }
      filtered.forEach((sound, index) => {
        const button = node("button", "program-row");
        const number = node(
          "span",
          "program-number",
          String(index + 1).padStart(3, "0"),
        );
        const name = node("span", "program-name");
        name.append(
          node("strong", "", sound.name),
          node("small", "", sound.detail ?? "CUSTOM PROGRAM"),
        );
        button.append(number, name, node("span", "program-status", "EDIT"));
        button.addEventListener("click", () =>
          request("plugin.begin_program_edit", { program_id: sound.id }),
        );
        list.append(button);
      });
    }
    search.addEventListener("input", drawList);
    drawList();
  }

  function valueText(value, kind) {
    if (value.type === "inherited") return "INHERITED";
    if (value.type === "boolean") return value.value ? "ON" : "OFF";
    if (value.type === "choice" || value.type === "sound_id") return value.value;
    const decimals = kind.decimals ?? 0;
    const scale = 10 ** decimals;
    return `${(value.value / scale).toFixed(decimals)}${kind.unit ? ` ${kind.unit}` : ""}`;
  }

  function renderToggleField(draft, field, value, disabled) {
    const control = node("button", `toggle-field${value.value ? " active" : ""}`);
    control.type = "button";
    control.disabled = disabled;
    control.setAttribute("role", "switch");
    control.setAttribute("aria-checked", String(value.value));
    control.append(
      node("span", "toggle-track"),
      node("strong", "", value.value ? "ON" : "OFF"),
    );
    control.addEventListener("click", () =>
      commitField(draft, field, {
        type: "boolean",
        value: !value.value,
      }),
    );
    return control;
  }

  function renderNumberField(draft, field, value, disabled) {
    const wrapper = node("div", "number-field");
    const inherited = value.type === "inherited";
    if (field.kind.allow_inherited) {
      const inheritButton = node(
        "button",
        `inherit-button${inherited ? " active" : ""}`,
        inherited ? "INHERITED" : "OVERRIDE",
      );
      inheritButton.type = "button";
      inheritButton.disabled = disabled;
      inheritButton.addEventListener("click", () => {
        if (inherited) {
          const initial = Math.min(
            field.kind.maximum,
            Math.max(field.kind.minimum, 0),
          );
          commitField(draft, field, { type: "integer", value: initial });
        } else {
          commitField(draft, field, { type: "inherited" });
        }
      });
      wrapper.append(inheritButton);
    }
    const rawValue = inherited
      ? Math.min(field.kind.maximum, Math.max(field.kind.minimum, 0))
      : value.value;
    const rangeRow = node("div", "range-row");
    const range = node("input");
    range.type = "range";
    range.min = String(field.kind.minimum);
    range.max = String(field.kind.maximum);
    range.step = String(field.kind.step);
    range.value = String(rawValue);
    range.disabled = disabled || inherited;
    range.setAttribute("aria-label", field.label);
    const output = node(
      "output",
      "",
      valueText({ type: "integer", value: rawValue }, field.kind),
    );
    range.addEventListener("input", () => {
      const next = { type: "integer", value: Number(range.value) };
      output.textContent = valueText(next, field.kind);
      if (field.live_preview) previewField(draft, field, next);
      else {
        localFieldValues.set(field.id, next);
        updateFieldVisual(field.id);
      }
    });
    range.addEventListener("change", () =>
      commitField(draft, field, {
        type: "integer",
        value: Number(range.value),
      }),
    );
    rangeRow.append(range, output);
    wrapper.append(rangeRow);
    return wrapper;
  }

  function renderChoiceField(draft, field, value, disabled) {
    const select = node("select", "choice-field");
    select.disabled = disabled;
    select.setAttribute("aria-label", field.label);
    field.kind.options.forEach((option) => {
      const element = node("option", "", option.label);
      element.value = option.value;
      element.selected = option.value === value.value;
      select.append(element);
    });
    select.addEventListener("change", () =>
      commitField(draft, field, {
        type: "choice",
        value: select.value,
      }),
    );
    return select;
  }

  function renderSoundField(draft, field, value, disabled, instance) {
    const select = node("select", "choice-field sound-field");
    select.disabled = disabled;
    select.setAttribute("aria-label", field.label);
    const sounds = instance.sounds.filter(
      (sound) =>
        !field.kind.bank ||
        (sound.bank ?? "").toLowerCase() === field.kind.bank.toLowerCase(),
    );
    sounds.forEach((sound) => {
      const element = node("option", "", sound.name);
      element.value = sound.id;
      element.selected = sound.id === value.value;
      select.append(element);
    });
    select.addEventListener("change", () =>
      commitField(draft, field, {
        type: "sound_id",
        value: select.value,
      }),
    );
    return select;
  }

  function renderEditorField(draft, field, disabled, instance) {
    const value = displayedFieldValue(field);
    const row = node("div", `editor-field${disabled ? " disabled" : ""}`);
    const copy = node("div", "editor-field-copy");
    copy.append(
      node("strong", "", field.label),
      node("small", "", field.detail),
    );
    let control;
    if (field.kind.type === "toggle" && value.type === "boolean") {
      control = renderToggleField(draft, field, value, disabled);
    } else if (
      field.kind.type === "number" &&
      (value.type === "integer" || value.type === "inherited")
    ) {
      control = renderNumberField(draft, field, value, disabled);
    } else if (field.kind.type === "choice" && value.type === "choice") {
      control = renderChoiceField(draft, field, value, disabled);
    } else if (field.kind.type === "sound" && value.type === "sound_id") {
      control = renderSoundField(draft, field, value, disabled, instance);
    } else {
      control = node("span", "invalid-field", "UNAVAILABLE");
    }
    row.append(copy, control);
    return row;
  }

  function renderEditorPage(
    draft,
    page,
    instance,
    parentEnabled,
    editable,
    depth = 0,
  ) {
    const effectiveEnabled = parentEnabled && page.enabled;
    const section = node(
      "section",
      `editor-section depth-${Math.min(depth, 2)}${effectiveEnabled ? "" : " disabled"}`,
    );
    const heading = node("header", "editor-section-heading");
    const copy = node("div");
    copy.append(
      node("h3", "", page.label),
      node("p", "", page.detail),
    );
    heading.append(
      copy,
      node("span", "section-scope", effectiveEnabled ? "ACTIVE" : "DISABLED"),
    );
    section.append(heading);

    if (page.id.endsWith(".amp-env") || page.id.endsWith(".pitch-env")) {
      section.append(renderEnvelopeVisual(page));
    } else if (page.id.endsWith(".lfo")) {
      section.append(renderLfoVisual(page));
    } else if (page.id.endsWith(".range")) {
      section.append(renderRangeVisual(page));
    }

    const fields = node("div", "editor-fields");
    (page.fields ?? []).forEach((field) =>
      fields.append(
        renderEditorField(
          draft,
          field,
          !parentEnabled || !editable,
          instance,
        ),
      ),
    );
    if (fields.childElementCount) section.append(fields);

    const children = node("div", "editor-subsections");
    (page.pages ?? []).forEach((child) =>
      children.append(
        renderEditorPage(
          draft,
          child,
          instance,
          effectiveEnabled,
          editable,
          depth + 1,
        ),
      ),
    );
    if (children.childElementCount) section.append(children);
    return section;
  }

  function renderProgramEditor(instance, draft) {
    root.replaceChildren();
    root.append(pluginHeader(instance, "CONFIG SURFACE"));

    if (activeDraftId !== draft.draft_id) {
      activeDraftId = draft.draft_id;
      editMode = false;
      localProgramName = null;
      localFieldValues.clear();
      discardPrompt = false;
      bridgeError = "";
      activeEditorPage = draft.editor.pages[0]?.id ?? "layer-a";
      focusProgramName = false;
    }
    if (localProgramName === draft.name) localProgramName = null;

    const editorHeader = node("section", "program-editor-header");
    const nameGroup = node("label", "program-name-editor");
    nameGroup.append(node("span", "eyebrow", "PROGRAM NAME"));
    const nameInput = node("input");
    nameInput.type = "text";
    nameInput.maxLength = 64;
    nameInput.value = localProgramName ?? draft.name;
    nameInput.readOnly = !editMode;
    nameInput.className = editMode ? "" : "locked";
    nameInput.setAttribute("aria-label", "Program name");
    const nameError = node("small", "program-name-error");
    const saveName = () => {
      const name = nameInput.value.trim();
      if (!name) {
        nameError.textContent = "A program name is required.";
        nameInput.setAttribute("aria-invalid", "true");
        return false;
      }
      if (!/^[\x20-\x7e]+$/.test(name)) {
        nameError.textContent = "Use printable ASCII characters only.";
        nameInput.setAttribute("aria-invalid", "true");
        return false;
      }
      nameError.textContent = "";
      nameInput.removeAttribute("aria-invalid");
      if (name === draft.name) return true;
      localProgramName = name;
      request("plugin.set_program_name", {
        draft_id: draft.draft_id,
        name,
      });
      return true;
    };
    nameInput.addEventListener("input", () => {
      nameError.textContent = "";
      nameInput.removeAttribute("aria-invalid");
    });
    nameInput.addEventListener("change", saveName);
    nameInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        nameInput.blur();
      }
    });
    const nameRow = node("div", "program-name-row");
    nameRow.append(nameInput);
    if (!editMode) {
      const rename = node("button", "rename-program-button", "✎ RENAME");
      rename.type = "button";
      rename.addEventListener("click", () => {
        editMode = true;
        focusProgramName = true;
        renderProgramEditor(instance, draft);
      });
      nameRow.append(rename);
    }
    nameGroup.append(nameRow, nameError);

    const actions = node("div", "editor-actions");
    const editToggle = node(
      "button",
      `edit-mode-toggle${editMode ? " active" : ""}`,
      editMode ? "✎ EDITING" : "✎ EDIT MODE",
    );
    editToggle.type = "button";
    editToggle.setAttribute("aria-pressed", String(editMode));
    editToggle.addEventListener("click", () => {
      if (editMode) {
        previewTimers.forEach((timer) => window.clearTimeout(timer));
        previewTimers.clear();
        request("plugin.restore_program_preview", {
          draft_id: draft.draft_id,
        });
      }
      editMode = !editMode;
      renderProgramEditor(instance, draft);
    });
    const status = node(
      "span",
      `draft-status${draft.dirty ? " dirty" : ""}`,
      draft.dirty ? "UNSAVED CHANGES" : "SAVED",
    );
    const cancel = node("button", "editor-secondary", "CANCEL");
    cancel.type = "button";
    cancel.addEventListener("click", () => {
      if (draft.dirty) {
        discardPrompt = true;
        renderProgramEditor(instance, draft);
      } else {
        request("plugin.cancel_program", { draft_id: draft.draft_id });
      }
    });
    const save = node("button", "editor-primary", "SAVE PROGRAM");
    save.type = "button";
    save.addEventListener("click", () => {
      if (!saveName()) return;
      request("plugin.save_program", { draft_id: draft.draft_id });
    });
    actions.append(editToggle, status, cancel, save);
    editorHeader.append(nameGroup, actions);
    root.append(editorHeader);

    if (discardPrompt) {
      const prompt = node("section", "discard-prompt");
      const copy = node("div");
      copy.append(
        node("strong", "", "Discard unsaved changes?"),
        node("p", "", "The saved program will remain unchanged."),
      );
      const buttons = node("div");
      const keep = node("button", "editor-secondary", "KEEP EDITING");
      keep.type = "button";
      keep.addEventListener("click", () => {
        discardPrompt = false;
        renderProgramEditor(instance, draft);
      });
      const discard = node("button", "editor-danger", "DISCARD");
      discard.type = "button";
      discard.addEventListener("click", () =>
        request("plugin.cancel_program", { draft_id: draft.draft_id }),
      );
      buttons.append(keep, discard);
      prompt.append(copy, buttons);
      root.append(prompt);
    }

    if (bridgeError) {
      root.append(node("p", "bridge-error", bridgeError));
    }

    editorVisuals.clear();
    const workspace = node(
      "div",
      `program-editor-workspace${editMode ? "" : " locked"}`,
    );
    const navigation = node("nav", "editor-page-navigation");
    navigation.setAttribute("aria-label", "Program editor sections");
    const currentPage =
      draft.editor.pages.find((page) => page.id === activeEditorPage) ??
      draft.editor.pages[0];
    if (currentPage) activeEditorPage = currentPage.id;
    draft.editor.pages.forEach((page) => {
      const button = node(
        "button",
        `editor-page-button${page.id === activeEditorPage ? " active" : ""}${page.enabled ? "" : " disabled"}`,
      );
      button.type = "button";
      button.append(
        node("span", "editor-page-mark", page.label.slice(0, 2)),
        node("strong", "", page.label),
        node("small", "", page.detail),
      );
      button.addEventListener("click", () => {
        activeEditorPage = page.id;
        renderProgramEditor(instance, draft);
      });
      navigation.append(button);
    });
    const pages = node("div", "program-editor-pages");
    if (currentPage) {
      const pageIntro = node("header", "active-page-intro");
      const introCopy = node("div");
      introCopy.append(
        node("span", "eyebrow", "PROGRAM SECTION"),
        node("h2", "", currentPage.label),
        node("p", "", currentPage.detail),
      );
      const editState = node(
        "span",
        `page-edit-state${editMode ? " active" : ""}`,
        editMode ? "EDITING LIVE" : "READ ONLY",
      );
      pageIntro.append(introCopy, editState);
      pages.append(
        pageIntro,
        renderEditorPage(draft, currentPage, instance, true, editMode),
      );
    }
    workspace.append(navigation, pages);
    root.append(workspace);
    if (focusProgramName) {
      focusProgramName = false;
      window.requestAnimationFrame(() => {
        nameInput.focus();
        nameInput.select();
      });
    }
  }

  function renderConfig(instance) {
    const draft = context.program_draft;
    if (draft) renderProgramEditor(instance, draft);
    else {
      activeDraftId = null;
      editMode = false;
      localProgramName = null;
      localFieldValues.clear();
      discardPrompt = false;
      editorVisuals.clear();
      renderConfigLibrary(instance);
    }
  }

  function render() {
    if (!context) return;
    if (surface === "config") renderConfig(context.instance);
    else renderPlay(context.instance);
  }

  window.addEventListener("message", (event) => {
    if (
      event.source !== window.parent ||
      event.origin !== hostOrigin ||
      !event.data ||
      event.data.protocol !== PROTOCOL
    ) {
      return;
    }
    if (event.data.kind === "context") {
      const previousSelection = context?.instance?.selected_sound_id;
      context = event.data;
      if (previousSelection !== context.instance.selected_sound_id) {
        activeCollection = collectionFor(selectedSound(context.instance));
      }
      render();
    } else if (event.data.kind === "response" && !event.data.ok) {
      bridgeError = event.data.error || "RackForge rejected this change.";
      render();
    }
  });

  window.parent.postMessage(
    { protocol: PROTOCOL, kind: "ready" },
    hostOrigin,
  );
})();
