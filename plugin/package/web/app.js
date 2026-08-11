(() => {
  "use strict";

  const PROTOCOL = "rackforge.plugin.web@1";
  const surface = document.body.dataset.surface;
  const root = document.getElementById("plugin-root");
  const hostOrigin = window.location.origin;
  let context = null;
  let requestSerial = 0;
  let search = "";
  let openLibrary = null;

  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = text;
    return element;
  }

  function request(method, params) {
    requestSerial += 1;
    window.parent.postMessage({
      protocol: PROTOCOL,
      kind: "request",
      request_id: `rf-soundfonts-${requestSerial}`,
      method,
      params,
    }, hostOrigin);
  }

  function selectedSound(instance) {
    return instance.sounds.find((sound) => sound.id === instance.selected_sound_id);
  }

  function librariesOf(instance) {
    const libraries = new Map();
    (instance.banks ?? []).forEach((bank) => {
      libraries.set(bank.id, {
        id: bank.id,
        name: bank.name,
        order: bank.order ?? 0,
        sounds: [],
      });
    });
    instance.sounds.forEach((sound) => {
      const id = sound.bank ?? "";
      if (!libraries.has(id)) {
        libraries.set(id, {
          id,
          name: id || "Uncategorized",
          order: Number.MAX_SAFE_INTEGER,
          sounds: [],
        });
      }
      libraries.get(id).sounds.push(sound);
    });
    return [...libraries.values()]
      .filter((library) => library.sounds.length > 0)
      .sort((left, right) => left.order - right.order);
  }

  function header(instance, label) {
    const element = node("header", "plugin-header");
    const copy = node("div");
    copy.append(
      node("span", "eyebrow", label),
      node("h1", "", instance.plugin_name),
      node("p", "", `${instance.sounds.length} sounds · ${librariesOf(instance).length} libraries`),
    );
    element.append(copy, node("span", "api-badge", "WEB API 1"));
    return element;
  }

  function soundRow(instance, sound) {
    const button = node(
      "button",
      `sound-row${sound.id === instance.selected_sound_id ? " selected" : ""}`,
    );
    button.type = "button";
    const copy = node("span", "sound-copy");
    copy.append(
      node("strong", "", sound.name),
      node("small", "", sound.detail ?? "LIBRARY INSTRUMENT"),
    );
    button.append(
      copy,
      node("span", "sound-status", sound.id === instance.selected_sound_id ? "PLAYING" : "LOAD"),
    );
    button.addEventListener("click", () => {
      request("plugin.select_sound", { sound_id: sound.id });
    });
    return button;
  }

  function renderPlay(instance) {
    root.replaceChildren();
    root.append(header(instance, "PLAY"));
    const current = selectedSound(instance);
    const now = node("section", "current-sound");
    const copy = node("div");
    copy.append(
      node("span", "eyebrow", current?.bank ?? "NO LIBRARY"),
      node("h2", "", current?.name ?? "No sound selected"),
      node("p", "", current?.detail ?? "Choose a library instrument below."),
    );
    now.append(copy, node("span", "ready", "MIDI READY"));
    root.append(now);

    const browser = node("section", "browser");
    const toolbar = node("div", "toolbar");
    const input = node("input");
    input.type = "search";
    input.value = search;
    input.placeholder = "Search library sounds";
    input.setAttribute("aria-label", "Search library sounds");
    const count = node("span", "count");
    const body = node("div", "browser-body");
    toolbar.append(input, count);
    browser.append(toolbar, body);
    root.append(browser);

    const draw = () => {
      search = input.value;
      const query = search.trim().toLowerCase();
      const libraries = librariesOf(instance);
      body.replaceChildren();
      if (query) {
        const matches = instance.sounds.filter((sound) =>
          sound.name.toLowerCase().includes(query)
          || (sound.detail ?? "").toLowerCase().includes(query));
        count.textContent = `${matches.length} SOUNDS`;
        matches.forEach((sound) => body.append(soundRow(instance, sound)));
        if (!matches.length) body.append(node("p", "empty", "No library sounds match."));
        return;
      }
      const library = libraries.find((candidate) => candidate.id === openLibrary);
      if (library) {
        count.textContent = `${library.sounds.length} SOUNDS`;
        const back = node("button", "back", `←  ${library.name}`);
        back.type = "button";
        back.addEventListener("click", () => {
          openLibrary = null;
          draw();
        });
        body.append(back);
        library.sounds.forEach((sound) => body.append(soundRow(instance, sound)));
        return;
      }
      count.textContent = `${libraries.length} LIBRARIES`;
      libraries.forEach((entry) => {
        const button = node("button", "library-row");
        button.type = "button";
        const copy = node("span");
        copy.append(
          node("strong", "", entry.name),
          node("small", "", `${entry.sounds.length} sounds`),
        );
        button.append(copy, node("span", "", "OPEN →"));
        button.addEventListener("click", () => {
          openLibrary = entry.id;
          draw();
        });
        body.append(button);
      });
    };
    input.addEventListener("input", draw);
    draw();
  }

  function renderConfig(instance) {
    root.replaceChildren();
    root.append(header(instance, "CONFIG"));
    const intro = node("section", "config-intro");
    intro.append(
      node("span", "eyebrow", "LIBRARY SETUP"),
      node("h2", "", "Installed sound resources"),
      node(
        "p",
        "",
        "RF-Soundfonts plays instruments exactly as they are provided by its installed libraries. Layers, splits and complete performance setups belong to the RackForge rack.",
      ),
    );
    root.append(intro);
    const list = node("section", "browser config-list");
    const libraries = librariesOf(instance);
    libraries.forEach((library) => {
      const row = node("div", "library-row static");
      const copy = node("span");
      copy.append(
        node("strong", "", library.name),
        node("small", "", `${library.sounds.length} sounds available`),
      );
      row.append(copy, node("span", "installed", "INSTALLED"));
      list.append(row);
    });
    if (!libraries.length) {
      list.append(node("p", "empty", "No sound resources are installed."));
    }
    root.append(list);
  }

  function render() {
    if (!context) return;
    if (surface === "config") renderConfig(context.instance);
    else renderPlay(context.instance);
  }

  window.addEventListener("message", (event) => {
    if (
      event.source !== window.parent
      || event.origin !== hostOrigin
      || event.data?.protocol !== PROTOCOL
    ) return;
    if (event.data.kind === "context") {
      context = event.data;
      render();
    }
  });

  window.parent.postMessage({ protocol: PROTOCOL, kind: "ready" }, hostOrigin);
})();
