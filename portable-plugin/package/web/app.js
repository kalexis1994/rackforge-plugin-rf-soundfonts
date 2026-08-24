(() => {
  "use strict";

  const PROTOCOL = "rackforge.plugin.web@1";
  const hostOrigin = window.location.origin;
  const pending = new Map();
  let requestSerial = 0;

  const requestTimeout = (method) => {
    if (method === "plugin.select_resource") return 5 * 60_000;
    if (method === "plugin.install_resource" || method === "plugin.clear_resource"
        || method === "plugin.preview_resource") {
      return 2 * 60_000;
    }
    return 20_000;
  };

  const hostRequest = (method, params = {}, transfer = []) =>
    new Promise((resolve, reject) => {
      const requestId = `rf-soundfonts-${Date.now()}-${requestSerial += 1}`;
      const timer = window.setTimeout(() => {
        pending.delete(requestId);
        reject(new Error("RackForge did not answer in time."));
      }, requestTimeout(method));
      pending.set(requestId, { resolve, reject, timer });
      window.parent.postMessage({
        protocol: PROTOCOL,
        kind: "request",
        request_id: requestId,
        method,
        params,
      }, hostOrigin, transfer);
    });

  window.addEventListener("pagehide", () => {
    for (const request of pending.values()) {
      window.clearTimeout(request.timer);
      request.reject(new Error("The plugin surface was closed."));
    }
    pending.clear();
  });

  // CONFIG uses tabs. PLAY keeps browser and rack visible at the same time.
  const tabs = [...document.querySelectorAll("[role=tab]")];
  const showTab = (name) => {
    for (const tab of tabs) {
      const selected = tab.dataset.tab === name;
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
      const panel = document.querySelector(`[data-panel="${tab.dataset.tab}"]`);
      if (!panel) continue;
      panel.hidden = !selected;
      panel.toggleAttribute("data-active", selected);
    }
  };
  for (const [index, tab] of tabs.entries()) {
    tab.addEventListener("click", () => showTab(tab.dataset.tab));
    tab.addEventListener("keydown", (event) => {
      const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
      if (!step) return;
      event.preventDefault();
      const next = tabs[(index + step + tabs.length) % tabs.length];
      showTab(next.dataset.tab);
      next.focus();
    });
  }

  // PLAY: searchable library and persistent instrument rack.
  const soundTitle = document.querySelector("[data-sound-title]");
  const soundSubtitle = document.querySelector("[data-sound-subtitle]");
  const soundBrowser = document.querySelector("[data-sound-browser]");
  const soundList = document.querySelector("[data-sound-list]");
  const soundCount = document.querySelector("[data-sound-count]");
  const soundSearch = document.querySelector("[data-sound-search]");
  const playStatus = document.querySelector("[data-play-status]");
  const activeBank = document.querySelector("[data-active-bank]");
  const activeFormat = document.querySelector("[data-active-format]");
  const instrumentRack = document.querySelector(".instrument-rack");
  const volume = document.querySelector("[data-volume]");
  const volumeValue = document.querySelector("[data-volume-value]");
  const fxCard = document.querySelector("[data-fx-card]");
  const fxLabel = document.querySelector("[data-fx-label]");
  const fxToggle = document.querySelector("[data-fx-toggle]");
  const fxAction = document.querySelector("[data-fx-action]");
  const fxEditor = document.querySelector("[data-fx-editor]");
  const fxParameters = document.querySelector("[data-fx-parameters]");
  const fxReset = document.querySelector("[data-fx-reset]");
  const fxAmount = document.querySelector("[data-fx-amount]");
  const fxAmountValue = document.querySelector("[data-fx-amount-value]");
  const bankShell = document.querySelector("[data-bank-shell]");
  const bankArtwork = document.querySelector("[data-bank-artwork]");
  const bankEdition = document.querySelector("[data-bank-edition]");
  const bankKicker = document.querySelector("[data-bank-kicker]");
  const bankControlKicker = document.querySelector("[data-bank-control-kicker]");
  const bankControlTitle = document.querySelector("[data-bank-control-title]");
  const bankEngine = document.querySelector("[data-bank-engine]");
  const bankFooter = document.querySelector("[data-bank-footer]");
  const bankCredit = document.querySelector("[data-bank-credit]");
  let currentInstance = null;
  let soundFilter = "";
  let soundSelectionBusy = false;
  let librarySelectionBusy = false;
  let contextRevision = 0;
  let pendingLibraryActivation = null;
  let addedLibraries = [];
  const expandedLibraries = new Set();
  const collapsedLibraries = new Set();
  const LIBRARY_CATALOGS_KEY = "rf-soundfonts.library-catalogs.v1";
  let activeGrantId = (() => {
    try {
      return window.localStorage.getItem("rf-soundfonts.active-library");
    } catch (_) {
      return null;
    }
  })();
  let libraryCatalogs = (() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(LIBRARY_CATALOGS_KEY) || "{}");
      return stored && typeof stored === "object" && !Array.isArray(stored) ? stored : {};
    } catch (_) {
      return {};
    }
  })();

  const rememberActiveLibrary = (grant) => {
    activeGrantId = grant?.grant_id || null;
    try {
      if (activeGrantId) {
        window.localStorage.setItem("rf-soundfonts.active-library", activeGrantId);
      } else {
        window.localStorage.removeItem("rf-soundfonts.active-library");
      }
    } catch (_) {
      // Private browsing can make local storage unavailable; playback still works.
    }
  };

  // PLAY is a safe renderer, not the identity of every bank. A bank profile
  // can select one of the supported compositions and supply its own artwork,
  // palette and copy, but it cannot inject markup, script or arbitrary CSS.
  const BANK_LAYOUTS = new Set(["studio", "cinematic", "compact", "minimal"]);
  const BANK_MODULES = new Set(["artwork", "instrument", "controls"]);
  const ARTWORK_MARKER = "\u001eRF_ARTWORK=";
  const DEFAULT_BANK_PRESENTATION = {
    id: "rf-instrument",
    layout: "studio",
    theme: {
      ground: "#091413",
      surface: "#0d1f1c",
      accent: "#b0e4cc",
      structure: "#408a71",
    },
    copy: {
      edition: "RF INSTRUMENT",
      kicker: "LOADED INSTRUMENT",
      control_kicker: "CHANNEL",
      control_title: "Output",
      engine: "WASM · 96 VOICES",
      footer: "RACKFORGE INSTRUMENT",
      credit: "RF-SOUNDFONTS",
    },
    modules: ["instrument", "controls"],
  };
  let bankPresentations = {
    fallback: DEFAULT_BANK_PRESENTATION,
    profiles: [],
  };

  const cleanText = (value, fallback, maximum = 72) => {
    if (typeof value !== "string") return fallback;
    const cleaned = value.trim();
    return cleaned ? cleaned.slice(0, maximum) : fallback;
  };

  const cleanColor = (value, fallback) =>
    typeof value === "string" && /^#[0-9a-f]{6}$/i.test(value) ? value : fallback;

  const cleanArtwork = (value) => {
    if (typeof value !== "string" || value.includes("\\") || value.includes("://")) return null;
    if (/^\.\.\/branding\/[a-z0-9._/-]+$/i.test(value)) return value;
    if (/^banks\/[a-z0-9._/-]+$/i.test(value) && !value.includes("../")) return value;
    return null;
  };

  const cleanProfileReference = (value) =>
    typeof value === "string" &&
    /^banks\/[a-z0-9._-]+\/presentation\.json$/i.test(value)
      ? value
      : null;

  const cleanPresentation = (candidate, fallback = DEFAULT_BANK_PRESENTATION) => {
    const value = candidate && typeof candidate === "object" ? candidate : {};
    const theme = value.theme && typeof value.theme === "object" ? value.theme : {};
    const copy = value.copy && typeof value.copy === "object" ? value.copy : {};
    const match = value.match && typeof value.match === "object" ? value.match : {};
    const strings = (input) => Array.isArray(input)
      ? input.filter((item) => typeof item === "string" && item.trim()).slice(0, 24)
      : [];
    const modules = Array.isArray(value.modules)
      ? [...new Set(value.modules.filter((module) => BANK_MODULES.has(module)))]
      : fallback.modules;
    return {
      id: cleanText(value.id, fallback.id, 48),
      layout: BANK_LAYOUTS.has(value.layout) ? value.layout : fallback.layout,
      theme: {
        ground: cleanColor(theme.ground, fallback.theme.ground),
        surface: cleanColor(theme.surface, fallback.theme.surface),
        accent: cleanColor(theme.accent, fallback.theme.accent),
        structure: cleanColor(theme.structure, fallback.theme.structure),
      },
      artwork: cleanArtwork(value.artwork),
      artwork_alt: cleanText(value.artwork_alt, "", 120),
      copy: {
        edition: cleanText(copy.edition, fallback.copy.edition),
        kicker: cleanText(copy.kicker, fallback.copy.kicker),
        control_kicker: cleanText(copy.control_kicker, fallback.copy.control_kicker),
        control_title: cleanText(copy.control_title, fallback.copy.control_title),
        engine: cleanText(copy.engine, fallback.copy.engine),
        footer: cleanText(copy.footer, fallback.copy.footer),
        credit: cleanText(copy.credit, fallback.copy.credit),
      },
      modules: modules.length ? modules : fallback.modules,
      match: {
        bank_ids: strings(match.bank_ids),
        bank_name_contains: strings(match.bank_name_contains),
        sound_name_contains: strings(match.sound_name_contains),
      },
    };
  };

  const includesAny = (value, needles) => {
    const haystack = String(value ?? "").toLocaleLowerCase();
    return needles.some((needle) => haystack.includes(needle.toLocaleLowerCase()));
  };

  const soundMetadata = (sound) => {
    const detail = typeof sound?.detail === "string" ? sound.detail : "";
    const marker = detail.indexOf(ARTWORK_MARKER);
    if (marker < 0) return { detail, artwork: null };
    const artwork = detail.slice(marker + ARTWORK_MARKER.length);
    return {
      detail: detail.slice(0, marker),
      artwork: artwork.length <= 64 * 1024 &&
        /^data:image\/jpeg;base64,[a-z0-9+/=]+$/i.test(artwork)
        ? artwork
        : null,
    };
  };

  const presentationFor = (instance, sound) => {
    const bank = (Array.isArray(instance?.banks) ? instance.banks : [])
      .find((candidate) => candidate.id === sound?.bank);
    const profile = bankPresentations.profiles.find((profile) => {
      const match = profile.match;
      return (
        (match.bank_ids.length && match.bank_ids.includes(bank?.id ?? sound?.bank)) ||
        (match.bank_name_contains.length && includesAny(bank?.name, match.bank_name_contains)) ||
        (match.sound_name_contains.length && includesAny(sound?.name, match.sound_name_contains))
      );
    }) ?? bankPresentations.fallback;
    const artwork = soundMetadata(sound).artwork;
    const hasResidentPresentation =
      typeof sound?.id === "string" && sound.id.startsWith("nki.");
    if (!hasResidentPresentation) return profile;
    return {
      ...profile,
      artwork: artwork ?? profile.artwork,
      artwork_alt: cleanText(sound?.name, "RF-Soundfonts instrument", 120),
      modules: artwork ? [...new Set(["artwork", ...profile.modules])] : profile.modules,
      copy: {
        ...profile.copy,
        edition: "RF INSTRUMENT",
        engine: "RF RESIDENT ENGINE",
        credit: "RF-SOUNDFONTS",
      },
    };
  };

  const applyBankPresentation = (instance, sound) => {
    if (!bankShell) return;
    const presentation = presentationFor(instance, sound);
    bankShell.dataset.bankProfile = presentation.id;
    bankShell.dataset.bankLayout = presentation.layout;
    bankShell.style.setProperty("--bank-ground", presentation.theme.ground);
    bankShell.style.setProperty("--bank-surface", presentation.theme.surface);
    bankShell.style.setProperty("--bank-accent", presentation.theme.accent);
    bankShell.style.setProperty("--bank-structure", presentation.theme.structure);

    const enabled = new Set(presentation.modules);
    bankShell.toggleAttribute(
      "data-bank-has-artwork",
      Boolean(presentation.artwork && enabled.has("artwork")),
    );
    for (const module of bankShell.querySelectorAll("[data-bank-module]")) {
      const name = module.dataset.bankModule;
      module.hidden = !enabled.has(name);
    }
    if (bankArtwork) {
      const source = presentation.artwork;
      bankArtwork.hidden = !source || !enabled.has("artwork");
      bankArtwork.alt = presentation.artwork_alt;
      if (source && bankArtwork.getAttribute("src") !== source) {
        bankArtwork.src = source;
      }
    }
    if (bankEdition) bankEdition.textContent = presentation.copy.edition;
    if (bankKicker) bankKicker.textContent = presentation.copy.kicker;
    if (bankControlKicker) {
      bankControlKicker.textContent = presentation.copy.control_kicker;
      bankControlKicker.title = presentation.copy.control_kicker;
    }
    if (bankControlTitle) {
      bankControlTitle.textContent = presentation.copy.control_title;
      bankControlTitle.title = presentation.copy.control_title;
    }
    if (bankEngine) bankEngine.textContent = presentation.copy.engine;
    if (bankFooter) bankFooter.textContent = presentation.copy.footer;
    if (bankCredit) bankCredit.textContent = presentation.copy.credit;
  };

  if (bankArtwork) {
    bankArtwork.addEventListener("error", () => {
      bankArtwork.hidden = true;
      bankShell?.setAttribute("data-artwork-error", "");
    });
    bankArtwork.addEventListener("load", () => {
      bankShell?.removeAttribute("data-artwork-error");
    });
  }

  if (bankShell) {
    fetch("bank-presentations.json", { cache: "no-store" })
      .then((response) => {
        if (!response.ok) throw new Error("Bank presentation catalog is unavailable.");
        return response.json();
      })
      .then((catalog) => {
        if (catalog?.schema_version !== 1) throw new Error("Unsupported bank presentation schema.");
        const fallback = cleanPresentation(catalog.fallback);
        const references = Array.isArray(catalog.profiles)
          ? catalog.profiles.slice(0, 64).map(cleanProfileReference).filter(Boolean)
          : [];
        return Promise.all(references.map((reference) =>
          fetch(reference, { cache: "no-store" })
            .then((response) => {
              if (!response.ok) throw new Error("Bank profile is unavailable.");
              return response.json();
            })
            .then((profile) => profile?.schema_version === 1
              ? cleanPresentation(profile, fallback)
              : null)
            .catch(() => null)))
          .then((profiles) => {
            bankPresentations = {
              fallback,
              profiles: profiles.filter(Boolean),
            };
            if (currentInstance) renderSounds(currentInstance);
          });
      })
      .catch(() => {
        bankPresentations = {
          fallback: DEFAULT_BANK_PRESENTATION,
          profiles: [],
        };
      });
  }

  const setPlayStatus = (message) => {
    if (playStatus) playStatus.textContent = message;
  };

  const setFxExpanded = (expanded) => {
    if (!fxToggle || !fxEditor) return;
    fxToggle.setAttribute("aria-expanded", String(expanded));
    fxEditor.hidden = !expanded;
    fxCard?.toggleAttribute("data-expanded", expanded);
    bankShell?.toggleAttribute("data-fx-expanded", expanded);
    if (fxAction) fxAction.textContent = expanded ? "CLOSE" : "EDIT";
  };

  const buildEffectEditor = () => {
    if (!fxParameters || fxParameters.childElementCount) return;
    const fragment = document.createDocumentFragment();
    for (const effect of ["REVERB", "DELAY"]) {
      const section = document.createElement("section");
      section.className = "fx-effect-section";
      section.dataset.fxEffect = effect;
      const heading = document.createElement("strong");
      heading.textContent = `RF ${effect}`;
      section.append(heading);
      const controls = document.createElement("div");
      controls.className = "fx-effect-controls";
      for (const definition of FX_CONTROL_DEFINITIONS.filter(
        (candidate) => candidate.effect === effect,
      )) {
        const control = document.createElement("label");
        control.className = "fx-parameter";
        const copy = document.createElement("span");
        copy.textContent = definition.label;
        const input = document.createElement("input");
        input.type = "range";
        input.min = String(definition.minimum);
        input.max = String(definition.maximum);
        input.step = "0.01";
        input.value = String(definition.neutral);
        input.disabled = true;
        input.dataset.fxParameter = String(definition.index);
        const output = document.createElement("output");
        output.dataset.fxParameterValue = String(definition.index);
        output.textContent = formatEffectValue(definition, definition.neutral);
        control.append(copy, input, output);
        controls.append(control);
      }
      section.append(controls);
      fragment.append(section);
    }
    fxParameters.append(fragment);
  };

  const showEffectSections = (detail) => {
    const enabled = new Set(effectNames(detail));
    for (const section of fxParameters?.querySelectorAll("[data-fx-effect]") ?? []) {
      section.hidden = !enabled.has(section.dataset.fxEffect);
    }
  };

  const bankNameFor = (instance, sound) => {
    const banks = Array.isArray(instance?.banks) ? instance.banks : [];
    return banks.find((bank) => bank.id === sound?.bank)?.name
      ?? sound?.bank
      ?? (banks.length === 1 ? banks[0].name : null)
      ?? "Factory bank";
  };

  const effectNames = (detail) => {
    const match = String(detail ?? "").match(
      /FX ((?:Reverb|Delay)(?: → (?:Reverb|Delay))*)/i,
    );
    return match
      ? [...new Set(match[1].split("→").map((name) => name.trim().toUpperCase()))]
      : [];
  };

  const effectMixLabel = (detail) => {
    const effects = effectNames(detail);
    return effects.length ? `RF ${effects.join(" + ")}` : null;
  };

  const FX_CONTROL_DEFINITIONS = [
    { index: 2, effect: "REVERB", label: "SIZE", minimum: 0.5, maximum: 1.5, neutral: 1, format: "scale" },
    { index: 3, effect: "REVERB", label: "DECAY", minimum: 0.25, maximum: 4, neutral: 1, format: "scale" },
    { index: 4, effect: "REVERB", label: "PRE-DELAY", minimum: 0, maximum: 2, neutral: 1, format: "scale" },
    { index: 5, effect: "REVERB", label: "DAMPING", minimum: -0.5, maximum: 0.5, neutral: 0, format: "offset" },
    { index: 6, effect: "REVERB", label: "WIDTH", minimum: 0, maximum: 1.5, neutral: 1, format: "scale" },
    { index: 7, effect: "DELAY", label: "TIME", minimum: 0.25, maximum: 4, neutral: 1, format: "scale" },
    { index: 8, effect: "DELAY", label: "FEEDBACK", minimum: -0.5, maximum: 0.2, neutral: 0, format: "offset" },
    { index: 9, effect: "DELAY", label: "DAMPING", minimum: -0.5, maximum: 0.5, neutral: 0, format: "offset" },
    { index: 10, effect: "DELAY", label: "STEREO", minimum: 0, maximum: 1.5, neutral: 1, format: "scale" },
  ];

  const formatEffectValue = (definition, value) => {
    if (definition.format === "offset") {
      const percent = Math.round(value * 100);
      return `${percent > 0 ? "+" : ""}${percent}%`;
    }
    return `${Number(value).toFixed(2)}×`;
  };

  buildEffectEditor();
  fxToggle?.addEventListener("click", () => {
    setFxExpanded(fxToggle.getAttribute("aria-expanded") !== "true");
  });

  const effectiveActiveGrantId = () =>
    pendingLibraryActivation && contextRevision > pendingLibraryActivation.startedAtRevision
      ? pendingLibraryActivation.grantId
      : activeGrantId;

  const updatePlayBusy = () => {
    const busy = soundSelectionBusy || librarySelectionBusy;
    soundBrowser?.toggleAttribute("aria-busy", busy);
    instrumentRack?.toggleAttribute("aria-busy", busy);
    for (const button of soundList?.querySelectorAll("button") ?? []) {
      button.disabled = busy;
    }
  };

  const libraryName = (grant) => {
    const name = cleanText(grant?.display_name, "RF instrument library", 120);
    return name.replace(/\.(?:nki|sf2|sfz|rfbank)$/i, "");
  };

  const dependencyBundle = (name) => {
    if (/\.nki$/i.test(name || "")) return "nki_dependencies";
    if (/\.sfz$/i.test(name || "")) return "sfz_dependencies";
    return null;
  };

  const persistLibraryCatalogs = () => {
    try {
      window.localStorage.setItem(LIBRARY_CATALOGS_KEY, JSON.stringify(libraryCatalogs));
    } catch (_) {
      // Catalog memory is an enhancement. A library can always be opened again.
    }
  };

  const rememberLibraryCatalog = (instance) => {
    const grantId = effectiveActiveGrantId();
    if (!grantId || !Array.isArray(instance?.sounds)) return;
    libraryCatalogs[grantId] = {
      banks: (Array.isArray(instance.banks) ? instance.banks : [])
        .slice(0, 128)
        .map((bank) => ({ id: bank.id, name: bank.name })),
      sounds: instance.sounds.slice(0, 1024).map((sound) => ({
        id: sound.id,
        name: sound.name,
        bank: sound.bank,
        detail: soundMetadata(sound).detail,
      })),
      selected_sound_id: instance.selected_sound_id ?? null,
    };
    persistLibraryCatalogs();
  };

  const catalogForLibrary = (grantId) => {
    if (grantId === effectiveActiveGrantId() && currentInstance) return currentInstance;
    const catalog = libraryCatalogs[grantId];
    return catalog && Array.isArray(catalog.sounds) ? catalog : null;
  };

  const finishLibraryActivation = (instance) => {
    const activation = pendingLibraryActivation;
    if (!activation || contextRevision <= activation.startedAtRevision || !activation.responded) return;
    pendingLibraryActivation = null;
    rememberLibraryCatalog(instance);
    const target = activation.soundId
      ? instance.sounds?.find((sound) => sound.id === activation.soundId)
      : null;
    if (target && target.id !== instance.selected_sound_id) {
      selectSound(target);
    } else {
      setPlayStatus(Array.isArray(instance.sounds) && instance.sounds.length ? "READY" : "NO BANK");
    }
  };

  const activateLibrary = async (grant, soundId = null) => {
    if (!grant?.grant_id || librarySelectionBusy || soundSelectionBusy) return;
    if (grant.grant_id === effectiveActiveGrantId()) {
      const sound = currentInstance?.sounds?.find((candidate) => candidate.id === soundId);
      if (sound) await selectSound(sound);
      return;
    }
    librarySelectionBusy = true;
    const previousGrantId = activeGrantId;
    const activation = {
      grantId: grant.grant_id,
      soundId,
      startedAtRevision: contextRevision,
      responded: false,
    };
    pendingLibraryActivation = activation;
    updatePlayBusy();
    setPlayStatus("LOADING LIBRARY");
    try {
      await hostRequest("plugin.activate_resource", {
        target_resource_id: "user-soundfont",
        grant_id: grant.grant_id,
        entry_id: null,
        bundle: dependencyBundle(grant.display_name),
      });
      rememberActiveLibrary(grant);
      activation.responded = true;
      if (currentInstance) finishLibraryActivation(currentInstance);
    } catch (error) {
      activeGrantId = previousGrantId;
      pendingLibraryActivation = null;
      setPlayStatus("LIBRARY ERROR");
      if (soundSubtitle) {
        soundSubtitle.textContent =
          error instanceof Error ? error.message : "The library could not be activated.";
      }
    } finally {
      librarySelectionBusy = false;
      updatePlayBusy();
    }
  };

  const selectSound = async (sound) => {
    if (!currentInstance || soundSelectionBusy || sound.id === currentInstance.selected_sound_id) {
      return;
    }
    soundSelectionBusy = true;
    updatePlayBusy();
    setPlayStatus("LOADING");
    try {
      await hostRequest("plugin.select_sound", { sound_id: sound.id });
      currentInstance = { ...currentInstance, selected_sound_id: sound.id };
      renderSounds(currentInstance);
      hostRequest("plugin.set_surface_info", {
        label: "Sound",
        value: sound.name,
      }).catch(() => {
        // Compact surface information is optional on older hosts.
      });
    } catch (error) {
      setPlayStatus("SELECTION ERROR");
      if (soundSubtitle) {
        soundSubtitle.textContent =
          error instanceof Error ? error.message : "The sound could not be selected.";
      }
    } finally {
      soundSelectionBusy = false;
      updatePlayBusy();
    }
  };

  const renderSounds = (instance) => {
    if (!soundList || !soundBrowser) return;
    currentInstance = instance;
    const sounds = Array.isArray(instance?.sounds) ? instance.sounds : [];
    const banks = Array.isArray(instance?.banks) ? instance.banks : [];
    const selected = sounds.find((sound) => sound.id === instance?.selected_sound_id)
      ?? sounds[0]
      ?? null;

    rememberLibraryCatalog(instance);

    applyBankPresentation(instance, selected);

    if (soundTitle) soundTitle.textContent = selected?.name ?? "No instrument loaded";
    if (soundSubtitle) {
      soundSubtitle.textContent = soundMetadata(selected).detail
        ?? (sounds.length
          ? `${sounds.length} sounds available in the loaded bank.`
          : "Open CONFIG to add or restore an RF instrument.");
    }
    if (activeBank) activeBank.textContent = bankNameFor(instance, selected);
    if (activeFormat) {
      activeFormat.textContent = "RF INSTRUMENT";
    }
    if (fxCard) {
      const detail = soundMetadata(selected).detail ?? "";
      const label = effectMixLabel(detail);
      fxCard.hidden = !selected?.id?.startsWith("nki.") || !label;
      if (fxLabel && label) fxLabel.textContent = label;
      showEffectSections(detail);
      if (fxCard.hidden) setFxExpanded(false);
    }
    setPlayStatus(sounds.length ? "READY" : "NO BANK");

    const query = soundFilter.trim().toLocaleLowerCase();
    const activeLibraryId = effectiveActiveGrantId();
    const libraries = addedLibraries.map((grant) => ({
      id: grant.grant_id,
      grant,
      name: libraryName(grant),
      catalog: catalogForLibrary(grant.grant_id),
      factory: false,
    }));
    if (!activeLibraryId) {
      libraries.unshift({
        id: "factory",
        grant: null,
        name: banks.length === 1 ? banks[0].name : "RF Factory Library",
        catalog: instance,
        factory: true,
      });
    } else if (!libraries.some((library) => library.id === activeLibraryId)) {
      libraries.unshift({
        id: activeLibraryId,
        grant: null,
        name: bankNameFor(instance, selected),
        catalog: instance,
        factory: false,
      });
    }

    const visibleLibraries = libraries.flatMap((library) => {
      const catalogSounds = Array.isArray(library.catalog?.sounds) ? library.catalog.sounds : [];
      const catalogBanks = new Map(
        (Array.isArray(library.catalog?.banks) ? library.catalog.banks : [])
          .map((bank) => [bank.id, bank.name]),
      );
      const libraryMatches = !query || library.name.toLocaleLowerCase().includes(query);
      const variants = query && !libraryMatches
        ? catalogSounds.filter((sound) =>
          [sound.name, sound.detail, catalogBanks.get(sound.bank)]
            .some((value) => String(value ?? "").toLocaleLowerCase().includes(query)))
        : catalogSounds;
      return libraryMatches || variants.length ? [{ ...library, variants }] : [];
    });

    if (soundCount) {
      soundCount.textContent = query
        ? `${visibleLibraries.length} / ${libraries.length}`
        : `${libraries.length} ${libraries.length === 1 ? "LIBRARY" : "LIBRARIES"}`;
    }

    const fragment = document.createDocumentFragment();
    if (!visibleLibraries.length) {
      const empty = document.createElement("span");
      empty.className = "sound-bank";
      empty.textContent = libraries.length
        ? "No libraries or instruments match this search."
        : "No library loaded.";
      fragment.append(empty);
    } else {
      for (const library of visibleLibraries) {
        const active = library.id === (activeLibraryId ?? "factory");
        const variants = library.variants;
        const hasBranches = variants.length > 1;
        if (active && hasBranches && !collapsedLibraries.has(library.id)) {
          expandedLibraries.add(library.id);
        }
        const expanded = hasBranches && Boolean(query || expandedLibraries.has(library.id));
        const node = document.createElement("div");
        node.className = `library-node${active ? " active" : ""}${expanded ? " expanded" : ""}`;

        const row = document.createElement("button");
        row.type = "button";
        row.className = "library-tree-row";
        row.setAttribute("role", "treeitem");
        row.setAttribute("aria-level", "1");
        row.setAttribute("aria-selected", String(active));
        if (hasBranches) row.setAttribute("aria-expanded", String(expanded));

        const disclosure = document.createElement("span");
        disclosure.className = `tree-disclosure${hasBranches ? "" : " leaf"}`;
        disclosure.setAttribute("aria-hidden", "true");
        disclosure.textContent = hasBranches ? "›" : "RF";

        const copy = document.createElement("span");
        copy.className = "library-tree-copy";
        const title = document.createElement("strong");
        title.textContent = library.name;
        const meta = document.createElement("small");
        meta.textContent = variants.length > 1
          ? `${variants.length} variants`
          : variants.length === 1
            ? variants[0].name
            : "Open library";
        copy.append(title, meta);

        const state = document.createElement("span");
        state.className = "library-tree-state";
        state.textContent = active ? "ACTIVE" : hasBranches ? String(variants.length) : "";
        row.append(disclosure, copy, state);
        row.addEventListener("click", () => {
          if (hasBranches) {
            if (expandedLibraries.has(library.id)) {
              expandedLibraries.delete(library.id);
              collapsedLibraries.add(library.id);
            } else {
              expandedLibraries.add(library.id);
              collapsedLibraries.delete(library.id);
            }
            renderSounds(currentInstance);
            return;
          }
          const onlySound = variants[0] ?? null;
          if (library.factory || active) {
            if (onlySound) selectSound(onlySound);
          } else if (library.grant) {
            activateLibrary(library.grant, onlySound?.id ?? null);
          }
        });
        node.append(row);

        if (hasBranches && expanded) {
          const branch = document.createElement("div");
          branch.className = "library-tree-branch";
          branch.setAttribute("role", "group");
          for (const sound of variants) {
            const button = document.createElement("button");
            const soundActive = active && sound.id === selected?.id;
            button.type = "button";
            button.className = `sound-entry library-variant${soundActive ? " selected" : ""}`;
            button.setAttribute("role", "treeitem");
            button.setAttribute("aria-level", "2");
            button.setAttribute("aria-selected", String(soundActive));
            button.textContent = sound.name;
            button.addEventListener("click", () => {
              if (library.factory || active) selectSound(sound);
              else if (library.grant) activateLibrary(library.grant, sound.id);
            });
            branch.append(button);
          }
          node.append(branch);
        }
        fragment.append(node);
      }
    }
    soundList.replaceChildren(fragment);
    updatePlayBusy();
  };

  soundSearch?.addEventListener("input", () => {
    soundFilter = soundSearch.value;
    if (currentInstance) renderSounds(currentInstance);
  });

  const showVolume = (value) => {
    if (!volume || !volumeValue) return;
    volume.value = String(value);
    volumeValue.textContent = `${Math.round(value * 100)}%`;
  };

  const showFxAmount = (value) => {
    if (!fxAmount || !fxAmountValue) return;
    fxAmount.value = String(value);
    fxAmountValue.textContent = `${Math.round(value * 100)}%`;
  };

  const refreshVolume = () => {
    if (!volume) return Promise.resolve();
    return hostRequest("plugin.parameters", {})
      .then((snapshot) => {
        const values = Array.isArray(snapshot?.values) ? snapshot.values : [];
        const value = values.find((entry) => entry.index === 0);
        if (value && Number.isFinite(value.value)) {
          volume.disabled = false;
          showVolume(value.value);
        }
        const fxValue = values.find((entry) => entry.index === 1);
        if (fxAmount && fxValue && Number.isFinite(fxValue.value)) {
          fxAmount.disabled = false;
          showFxAmount(fxValue.value);
        }
        for (const definition of FX_CONTROL_DEFINITIONS) {
          const entry = values.find((candidate) => candidate.index === definition.index);
          const input = fxParameters?.querySelector(
            `[data-fx-parameter="${definition.index}"]`,
          );
          const output = fxParameters?.querySelector(
            `[data-fx-parameter-value="${definition.index}"]`,
          );
          if (input instanceof HTMLInputElement && entry && Number.isFinite(entry.value)) {
            input.disabled = false;
            input.value = String(entry.value);
            if (output) output.textContent = formatEffectValue(definition, entry.value);
          }
        }
      })
      .catch(() => {
        volume.disabled = true;
        if (fxAmount) fxAmount.disabled = true;
        for (const input of fxParameters?.querySelectorAll("input") ?? []) {
          input.disabled = true;
        }
      });
  };

  if (volume) {
    let sendTimer = null;
    let writeGeneration = 0;
    const sendVolume = () => {
      if (sendTimer) window.clearTimeout(sendTimer);
      sendTimer = null;
      const generation = writeGeneration += 1;
      hostRequest("plugin.set_parameter", {
        parameter_index: 0,
        value: Number(volume.value),
      }).catch(() => {
        if (generation === writeGeneration) {
          setPlayStatus("CONTROL ERROR");
          refreshVolume();
        }
      });
    };
    volume.addEventListener("input", () => {
      if (volumeValue) volumeValue.textContent = `${Math.round(Number(volume.value) * 100)}%`;
      if (sendTimer) window.clearTimeout(sendTimer);
      sendTimer = window.setTimeout(sendVolume, 80);
    });
    volume.addEventListener("change", sendVolume);
  }

  if (fxAmount) {
    let sendTimer = null;
    let writeGeneration = 0;
    const sendFxAmount = () => {
      if (sendTimer) window.clearTimeout(sendTimer);
      sendTimer = null;
      const generation = writeGeneration += 1;
      hostRequest("plugin.set_parameter", {
        parameter_index: 1,
        value: Number(fxAmount.value),
      }).catch(() => {
        if (generation === writeGeneration) {
          setPlayStatus("CONTROL ERROR");
          refreshVolume();
        }
      });
    };
    fxAmount.addEventListener("input", () => {
      if (fxAmountValue) {
        fxAmountValue.textContent = `${Math.round(Number(fxAmount.value) * 100)}%`;
      }
      if (sendTimer) window.clearTimeout(sendTimer);
      sendTimer = window.setTimeout(sendFxAmount, 80);
    });
    fxAmount.addEventListener("change", sendFxAmount);
  }

  if (fxParameters) {
    const sendTimers = new Map();
    const writeGenerations = new Map();
    const sendEffectParameter = (definition, input) => {
      const timer = sendTimers.get(definition.index);
      if (timer) window.clearTimeout(timer);
      sendTimers.delete(definition.index);
      const generation = (writeGenerations.get(definition.index) ?? 0) + 1;
      writeGenerations.set(definition.index, generation);
      hostRequest("plugin.set_parameter", {
        parameter_index: definition.index,
        value: Number(input.value),
      }).catch(() => {
        if (writeGenerations.get(definition.index) === generation) {
          setPlayStatus("CONTROL ERROR");
          refreshVolume();
        }
      });
    };
    for (const definition of FX_CONTROL_DEFINITIONS) {
      const input = fxParameters.querySelector(
        `[data-fx-parameter="${definition.index}"]`,
      );
      const output = fxParameters.querySelector(
        `[data-fx-parameter-value="${definition.index}"]`,
      );
      if (!(input instanceof HTMLInputElement)) continue;
      input.addEventListener("input", () => {
        if (output) output.textContent = formatEffectValue(definition, Number(input.value));
        const timer = sendTimers.get(definition.index);
        if (timer) window.clearTimeout(timer);
        sendTimers.set(
          definition.index,
          window.setTimeout(() => sendEffectParameter(definition, input), 80),
        );
      });
      input.addEventListener("change", () => sendEffectParameter(definition, input));
    }
  }

  fxReset?.addEventListener("click", async () => {
    const visible = FX_CONTROL_DEFINITIONS.filter((definition) => {
      const section = fxParameters?.querySelector(`[data-fx-effect="${definition.effect}"]`);
      return section && !section.hidden;
    });
    if (!visible.length) return;
    fxReset.disabled = true;
    setPlayStatus("RESETTING EFFECT");
    try {
      await Promise.all(visible.map((definition) => hostRequest("plugin.set_parameter", {
        parameter_index: definition.index,
        value: definition.neutral,
      })));
      await refreshVolume();
      setPlayStatus("READY");
    } catch (_) {
      setPlayStatus("CONTROL ERROR");
      await refreshVolume();
    } finally {
      fxReset.disabled = false;
    }
  });

  // CONFIG: direct instrument-file selection and installed-bank lifecycle.
  const selectLibrary = document.querySelector("[data-select-library]");
  const selection = document.querySelector("[data-library-selection]");
  const libraryEntries = document.querySelector("[data-library-entries]");
  const libraryCount = document.querySelector("[data-library-count]");
  const configStatus = document.querySelector("[data-config-status]");
  const factoryCard = document.querySelector("[data-factory-card]");
  const noBankCard = document.querySelector("[data-no-bank]");
  const clearBank = document.querySelector("[data-clear-bank]");
  const installedBankName = document.querySelector("[data-installed-bank-name]");
  const installedBankDetail = document.querySelector("[data-installed-bank-detail]");
  let installedName = null;

  const setConfigMessage = (message) => {
    if (selection) selection.textContent = message;
  };

  const setLibraryBusy = (busy) => {
    selectLibrary?.toggleAttribute("disabled", busy);
    for (const button of libraryEntries?.querySelectorAll("button") ?? []) {
      button.disabled = busy;
    }
  };

  const renderAddedLibraries = () => {
    if (!libraryEntries) return;
    if (libraryCount) {
      libraryCount.textContent = `${addedLibraries.length} ${addedLibraries.length === 1 ? "LIBRARY" : "LIBRARIES"}`;
    }
    if (!addedLibraries.length) {
      const empty = document.createElement("span");
      empty.className = "empty";
      empty.textContent = "No user libraries added yet.";
      libraryEntries.replaceChildren(empty);
      return;
    }
    const fragment = document.createDocumentFragment();
    for (const grant of addedLibraries) {
      const button = document.createElement("button");
      const active = grant.grant_id === activeGrantId;
      button.type = "button";
      button.className = `library-entry${active ? " selected" : ""}`;
      button.dataset.grantId = grant.grant_id;
      button.textContent = `${active ? "●" : "♪"}  ${grant.display_name || "User library"}${active ? "  ·  PLAYING" : ""}`;
      button.setAttribute("aria-pressed", active ? "true" : "false");
      button.addEventListener("click", () => installBank(grant));
      fragment.append(button);
    }
    libraryEntries.replaceChildren(fragment);
  };

  const refreshAddedLibraries = () =>
    hostRequest("plugin.resource_bindings", {})
      .then((grants) => {
        const values = Array.isArray(grants) ? grants : [];
        const byGrant = new Map();
        for (const grant of values) {
          if (grant?.resource_id !== "user-soundfont" || grant?.kind !== "file") continue;
          if (typeof grant.grant_id === "string" && grant.grant_id) {
            byGrant.set(grant.grant_id, grant);
          }
        }
        addedLibraries = [...byGrant.values()];
        const available = new Set(addedLibraries.map((grant) => grant.grant_id));
        libraryCatalogs = Object.fromEntries(
          Object.entries(libraryCatalogs).filter(([grantId]) => available.has(grantId)),
        );
        persistLibraryCatalogs();
        renderAddedLibraries();
        if (soundList && currentInstance) renderSounds(currentInstance);
        return addedLibraries;
      })
      .catch(() => {
        addedLibraries = [];
        renderAddedLibraries();
        if (soundList && currentInstance) renderSounds(currentInstance);
        return addedLibraries;
      });

  const showInstalled = (installed) => {
    if (factoryCard) factoryCard.hidden = !installed;
    if (noBankCard) noBankCard.hidden = installed;
    if (configStatus) configStatus.textContent = installed ? "USER LIBRARY ACTIVE" : "FACTORY LIBRARY ACTIVE";
    if (installedBankName) installedBankName.textContent = installedName || "Active user library";
    if (installedBankDetail) {
      installedBankDetail.textContent =
        "Stored privately by RackForge and loaded automatically on every start.";
    }
  };

  const refreshInstalled = () => {
    if (!factoryCard) return Promise.resolve(false);
    return hostRequest("plugin.resource_status", {})
      .then((statuses) => {
        const values = Array.isArray(statuses) ? statuses : [];
        const user = values.find((status) => status.resource_id === "user-soundfont");
        const installed = Boolean(user?.installed);
        showInstalled(installed);
        return installed;
      })
      .catch(() => false);
  };

  const installBank = async (grant) => {
    const name = grant?.display_name || "Selected instrument";
    setLibraryBusy(true);
    setConfigMessage(`Installing ${name}…`);
    if (configStatus) configStatus.textContent = "INSTALLING BANK";
    try {
      await hostRequest("plugin.install_resource", {
        target_resource_id: "user-soundfont",
        grant_id: grant.grant_id,
        entry_id: null,
        bundle: dependencyBundle(name),
      });
      installedName = name;
      rememberActiveLibrary(grant);
      renderAddedLibraries();
      setConfigMessage(`Added and playing: ${name}. You can add another library.`);
      await refreshInstalled();
    } catch (error) {
      setConfigMessage(error instanceof Error ? error.message : "The bank could not be installed.");
      if (configStatus) configStatus.textContent = "INSTALL FAILED";
    } finally {
      setLibraryBusy(false);
    }
  };

  selectLibrary?.addEventListener("click", async () => {
    selectLibrary.disabled = true;
    setConfigMessage("Waiting for RackForge…");
    try {
      const grant = await hostRequest("plugin.select_resource", {
        resource_id: "user-soundfont",
        extensions: ["nki", "sf2", "sfz", "rfbank"],
      });
      if (!grant?.grant_id) throw new Error("RackForge did not return a valid file grant.");
      if (!addedLibraries.some((candidate) => candidate.grant_id === grant.grant_id)) {
        addedLibraries.push(grant);
      }
      renderAddedLibraries();
      await installBank(grant);
    } catch (error) {
      setConfigMessage(error instanceof Error ? error.message : "File selection failed.");
    } finally {
      selectLibrary.disabled = false;
    }
  });

  if (clearBank) {
    let confirmTimer = null;
    const resetClearConfirmation = () => {
      if (confirmTimer) window.clearTimeout(confirmTimer);
      confirmTimer = null;
      clearBank.removeAttribute("data-confirming");
      clearBank.textContent = "Restore factory bank";
    };
    clearBank.addEventListener("click", async () => {
      if (!clearBank.hasAttribute("data-confirming")) {
        clearBank.setAttribute("data-confirming", "");
        clearBank.textContent = "Confirm restore";
        setConfigMessage("Click again to remove the installed bank and restore the factory piano.");
        confirmTimer = window.setTimeout(resetClearConfirmation, 6_000);
        return;
      }
      resetClearConfirmation();
      clearBank.disabled = true;
      if (configStatus) configStatus.textContent = "RESTORING FACTORY BANK";
      try {
        await hostRequest("plugin.clear_resource", {
          target_resource_id: "user-soundfont",
        });
        installedName = null;
        rememberActiveLibrary(null);
        renderAddedLibraries();
        showInstalled(false);
        setConfigMessage("Factory YDP Grand Piano restored. Your added libraries remain available.");
      } catch (error) {
        setConfigMessage(error instanceof Error ? error.message : "The bank could not be cleared.");
        if (configStatus) configStatus.textContent = "RESTORE FAILED";
      } finally {
        clearBank.disabled = false;
      }
    });
  }

  // CONFIG: native RF instrument builder. A zone owns its velocity layers and
  // may override the instrument envelope as a whole; individual keys do not
  // acquire hidden per-note state.
  const builderViewport = document.querySelector("[data-builder-viewport]");
  const builderMap = document.querySelector("[data-builder-map]");
  const builderKeys = document.querySelector("[data-builder-keys]");
  const builderLane = document.querySelector("[data-builder-lane]");
  const builderName = document.querySelector("[data-builder-name]");
  const builderSummary = document.querySelector("[data-builder-summary]");
  const builderMessage = document.querySelector("[data-builder-message]");
  const builderOpen = document.querySelector("[data-builder-open]");
  const builderOpenFile = document.querySelector("[data-builder-open-file]");
  const builderNewZone = document.querySelector("[data-builder-new-zone]");
  const builderSave = document.querySelector("[data-builder-save]");
  const builderExport = document.querySelector("[data-builder-export]");
  const builderSamples = document.querySelector("[data-builder-samples]");
  const builderBankContext = document.querySelector("[data-builder-bank-context]");
  const builderBankName = document.querySelector("[data-builder-bank-name]");
  const builderBankDetail = document.querySelector("[data-builder-bank-detail]");
  const builderAudition = document.querySelector("[data-builder-audition]");
  const builderInstrument = document.querySelector("[data-builder-instrument]");
  const builderNewBank = document.querySelector("[data-builder-new-bank]");
  const globalEnvelopeHost = document.querySelector("[data-global-envelope]");
  const zoneDialog = document.querySelector("[data-zone-dialog]");
  const zoneEnvelopeHost = document.querySelector("[data-zone-envelope]");
  const layerList = document.querySelector("[data-layer-list]");
  const BUILDER_DRAFT_KEY = "rf-soundfonts.instrument-builder.v1";
  const SAMPLE_LIMIT_BYTES = 160 * 1_048_576;
  const MAX_BANK_BYTES = 128 * 1_048_576;
  const MAX_BANK_ENTRIES = 8_192;
  const MAX_MAP_BYTES = 2 * 1_048_576;
  const MAX_VOLUME_PERCENT = 100 * (10 ** (24 / 20));
  let builderRowHeight = 28;
  let selectedZoneId = null;
  let editingZone = null;
  let sampleTargetLayerId = null;
  let builderDidInitialScroll = false;
  const builderFiles = new Map();
  const builderWaveforms = new Map();
  const ZONE_COLORS = ["#69c89e", "#67a8ff", "#ad82f2", "#ef8e9b", "#f4b765", "#62cbd0", "#d4d875", "#d98ac8"];
  const EDIT_MODE_OFF = 0;
  const EDIT_MODE_ON = 1;
  let suppressZoneClickUntil = 0;
  let openedBank = null;
  let builderEditMode = EDIT_MODE_OFF;
  let builderPreviewTimer = 0;
  let builderPreviewRevision = 0;
  let builderPreviewBusy = false;

  const defaultEnvelope = () => ({ attack: 0.01, decay: 0.2, sustain: 0.8, release: 0.4 });
  const defaultBuilderState = () => ({
    schema_version: 1,
    name: "My RF Instrument",
    envelope: defaultEnvelope(),
    zones: [],
  });
  let builderState = (() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(BUILDER_DRAFT_KEY) || "null");
      if (stored?.schema_version === 1 && Array.isArray(stored.zones)) return stored;
    } catch (_) {
      // A fresh draft is safer than attempting to repair malformed local data.
    }
    return defaultBuilderState();
  })();

  const builderId = (prefix) =>
    `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;

  const clampNumber = (value, low, high, fallback) => {
    const number = Number(value);
    return Number.isFinite(number) ? Math.min(high, Math.max(low, number)) : fallback;
  };

  const normalizeZoneColor = (value, fallback = ZONE_COLORS[0]) =>
    /^#[0-9a-f]{6}$/i.test(String(value || "")) ? String(value).toLowerCase() : fallback;

  const zoneColorRgb = (color) => {
    const normalized = normalizeZoneColor(color).slice(1);
    return `${Number.parseInt(normalized.slice(0, 2), 16)}, ${Number.parseInt(normalized.slice(2, 4), 16)}, ${Number.parseInt(normalized.slice(4, 6), 16)}`;
  };

  const noteName = (note) => {
    const names = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];
    return `${names[note % 12]}${Math.floor(note / 12) - 1}`;
  };

  const persistBuilder = () => {
    try {
      window.localStorage.setItem(BUILDER_DRAFT_KEY, JSON.stringify(builderState));
    } catch (_) {
      setBuilderMessage("The draft cannot be stored in this session.", "error");
    }
    if (builderEditMode === EDIT_MODE_ON) scheduleBuilderPreview();
  };

  const setBuilderMessage = (message, kind = "") => {
    if (!builderMessage) return;
    builderMessage.textContent = message;
    builderMessage.toggleAttribute("data-error", kind === "error");
    builderMessage.toggleAttribute("data-success", kind === "success");
  };

  const envelopeDefinitions = [
    { key: "attack", label: "Attack", min: 0, max: 30, step: 0.001, unit: "s" },
    { key: "decay", label: "Decay", min: 0, max: 30, step: 0.001, unit: "s" },
    { key: "sustain", label: "Sustain", min: 0, max: 1, step: 0.01, unit: "%" },
    { key: "release", label: "Release", min: 0, max: 30, step: 0.001, unit: "s" },
  ];

  const envelopeLabel = (definition, value) =>
    definition.unit === "%" ? `${Math.round(value * 100)}%` : `${value.toFixed(value < 0.1 ? 3 : 2)} s`;

  const renderEnvelope = (host, envelope, onChange) => {
    if (!host) return;
    const fragment = document.createDocumentFragment();
    for (const definition of envelopeDefinitions) {
      const label = document.createElement("label");
      label.className = "adsr-control";
      const caption = document.createElement("span");
      caption.textContent = definition.label;
      const output = document.createElement("output");
      const input = document.createElement("input");
      input.type = "range";
      input.min = definition.min;
      input.max = definition.max;
      input.step = definition.step;
      input.value = clampNumber(envelope[definition.key], definition.min, definition.max, 0);
      output.textContent = envelopeLabel(definition, Number(input.value));
      input.addEventListener("input", () => {
        const value = Number(input.value);
        envelope[definition.key] = value;
        output.textContent = envelopeLabel(definition, value);
        onChange?.();
      });
      label.append(caption, output, input);
      fragment.append(label);
    }
    host.replaceChildren(fragment);
  };

  const newLayer = (velocityLow = 1, velocityHigh = 127) => ({
    id: builderId("layer"),
    sample_name: "",
    velocity_low: velocityLow,
    velocity_high: velocityHigh,
    volume_percent: 100,
    pan: 0,
    tune_semitones: 0,
  });

  // Drafts created before volume was presented as a percentage keep their
  // exact audible level when opened in the newer card editor.
  for (const [zoneIndex, zone] of builderState.zones.entries()) {
    zone.color = normalizeZoneColor(zone.color, ZONE_COLORS[zoneIndex % ZONE_COLORS.length]);
    if (!Array.isArray(zone.layers)) zone.layers = [];
    for (const layer of zone.layers) {
      if (!Number.isFinite(layer.volume_percent)) {
        const gain = Number.isFinite(layer.gain_db) ? layer.gain_db : 0;
        layer.volume_percent = Math.round(100 * (10 ** (gain / 20)));
      }
      delete layer.gain_db;
    }
  }

  const newZone = (note = 60) => ({
    id: builderId("zone"),
    name: `Zone ${noteName(note)}`,
    key_low: note,
    key_high: note,
    root_key: note,
    color: ZONE_COLORS[builderState.zones.length % ZONE_COLORS.length],
    envelope_override: null,
    layers: [],
  });

  const formatWaveformDuration = (seconds) => {
    if (!Number.isFinite(seconds)) return "--:--";
    if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 2 : 1)} s`;
    const minutes = Math.floor(seconds / 60);
    return `${minutes}:${Math.floor(seconds % 60).toString().padStart(2, "0")}`;
  };

  const analyzeWaveform = async (layerId, file) => {
    builderWaveforms.set(layerId, { status: "loading" });
    renderLayerList();
    const AudioContextClass = window.AudioContext || window.webkitAudioContext;
    if (!AudioContextClass) {
      builderWaveforms.set(layerId, { status: "error", message: "Waveform preview is unavailable." });
      renderLayerList();
      return;
    }
    let context;
    try {
      context = new AudioContextClass();
      const audioBuffer = await context.decodeAudioData(await file.arrayBuffer());
      if (builderFiles.get(layerId) !== file) return;
      const bucketCount = Math.min(720, Math.max(96, Math.ceil(audioBuffer.duration * 90)));
      const channelPeaks = [];
      let peakAmplitude = 0;
      for (let channelIndex = 0; channelIndex < Math.min(audioBuffer.numberOfChannels, 2); channelIndex += 1) {
        const samples = audioBuffer.getChannelData(channelIndex);
        const peaks = new Float32Array(bucketCount * 2);
        for (let bucket = 0; bucket < bucketCount; bucket += 1) {
          const start = Math.floor((bucket * samples.length) / bucketCount);
          const end = Math.max(start + 1, Math.floor(((bucket + 1) * samples.length) / bucketCount));
          let minimum = 1;
          let maximum = -1;
          for (let frame = start; frame < Math.min(end, samples.length); frame += 1) {
            const sample = samples[frame];
            minimum = Math.min(minimum, sample);
            maximum = Math.max(maximum, sample);
          }
          if (minimum > maximum) minimum = maximum = 0;
          peaks[bucket * 2] = minimum;
          peaks[bucket * 2 + 1] = maximum;
          peakAmplitude = Math.max(peakAmplitude, Math.abs(minimum), Math.abs(maximum));
        }
        channelPeaks.push(peaks);
      }
      builderWaveforms.set(layerId, {
        status: "ready",
        channels: audioBuffer.numberOfChannels,
        channelPeaks,
        duration: audioBuffer.duration,
        sampleRate: audioBuffer.sampleRate,
        clipped: peakAmplitude >= 0.999,
      });
    } catch (_) {
      if (builderFiles.get(layerId) === file) {
        builderWaveforms.set(layerId, {
          status: "error",
          message: "This WAV could not be decoded for preview.",
        });
      }
    } finally {
      await context?.close?.().catch(() => {});
      if (builderFiles.get(layerId) === file) renderLayerList();
    }
  };

  const drawLayerWaveform = (canvas, waveform) => {
    if (!canvas?.isConnected || waveform?.status !== "ready" || !waveform.channelPeaks.length) return;
    const width = Math.max(280, Math.round(canvas.clientWidth || 620));
    const height = Math.max(58, Math.round(canvas.clientHeight || 72));
    const ratio = Math.min(2, window.devicePixelRatio || 1);
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
    const context = canvas.getContext("2d");
    context.scale(ratio, ratio);
    context.clearRect(0, 0, width, height);

    context.strokeStyle = "rgba(176, 228, 204, .08)";
    context.lineWidth = 1;
    for (let line = 1; line < 4; line += 1) {
      const x = Math.round((width * line) / 4) + .5;
      context.beginPath();
      context.moveTo(x, 0);
      context.lineTo(x, height);
      context.stroke();
    }

    const lanes = waveform.channelPeaks.length;
    const laneHeight = height / lanes;
    const gradient = context.createLinearGradient(0, 0, width, 0);
    gradient.addColorStop(0, "rgba(117, 174, 149, .78)");
    gradient.addColorStop(.42, "rgba(210, 255, 235, .98)");
    gradient.addColorStop(1, "rgba(114, 219, 181, .82)");
    context.strokeStyle = gradient;
    context.lineWidth = Math.max(1, Math.min(1.6, width / 500));
    for (const [channelIndex, peaks] of waveform.channelPeaks.entries()) {
      const center = laneHeight * channelIndex + laneHeight / 2;
      context.strokeStyle = "rgba(176, 228, 204, .12)";
      context.beginPath();
      context.moveTo(0, Math.round(center) + .5);
      context.lineTo(width, Math.round(center) + .5);
      context.stroke();
      context.strokeStyle = gradient;
      context.beginPath();
      const amplitude = laneHeight * .42;
      const bucketTotal = peaks.length / 2;
      for (let bucket = 0; bucket < bucketTotal; bucket += 1) {
        const x = (bucket / Math.max(1, bucketTotal - 1)) * width;
        context.moveTo(x, center - peaks[bucket * 2 + 1] * amplitude);
        context.lineTo(x, center - peaks[bucket * 2] * amplitude);
      }
      context.stroke();
    }
  };

  const createLayerWaveform = (layer) => {
    const shell = document.createElement("section");
    shell.className = "layer-waveform-shell";
    const waveform = builderWaveforms.get(layer.id);
    const file = builderFiles.get(layer.id);
    if (waveform?.status === "ready") {
      const canvas = document.createElement("canvas");
      canvas.className = "layer-waveform";
      canvas.setAttribute("aria-label", `Waveform preview for ${layer.sample_name}`);
      const metadata = document.createElement("div");
      metadata.className = "layer-waveform-meta";
      const format = document.createElement("span");
      format.textContent = `${waveform.channels === 1 ? "MONO" : `${waveform.channels} CH`} · ${(waveform.sampleRate / 1000).toFixed(1)} kHz · ${formatWaveformDuration(waveform.duration)}`;
      metadata.append(format);
      if (waveform.clipped) {
        const clipping = document.createElement("strong");
        clipping.textContent = "PEAK";
        clipping.title = "The sample reaches full scale.";
        metadata.append(clipping);
      }
      shell.append(canvas, metadata);
      requestAnimationFrame(() => drawLayerWaveform(canvas, waveform));
    } else {
      shell.classList.add("empty");
      const pulse = document.createElement("span");
      pulse.className = "layer-waveform-placeholder";
      const message = document.createElement("span");
      if (waveform?.status === "loading") {
        shell.classList.add("loading");
        message.textContent = "Analyzing waveform…";
      } else if (waveform?.status === "error") {
        shell.classList.add("error");
        message.textContent = waveform.message;
      } else if (layer.sample_name && !file) {
        message.textContent = "Reload the sample to restore its preview and include it in the export.";
      } else {
        message.textContent = "Load a WAV or FLAC sample to preview its waveform.";
      }
      shell.append(pulse, message);
    }
    return shell;
  };

  const zoneAtNote = (note) => {
    const candidates = builderState.zones.filter(
      (zone) => note >= zone.key_low && note <= zone.key_high,
    );
    if (!candidates.length) return null;
    return candidates.find((zone) => zone.id === selectedZoneId) || candidates.at(-1);
  };

  const openZoneForNote = (note) => {
    const zone = zoneAtNote(note);
    openZoneEditor(zone || newZone(note));
  };

  const coupledZoneAtEdge = (zone, edge) => {
    const candidates = builderState.zones.filter((candidate) => candidate.id !== zone.id && (
      edge === "high"
        ? candidate.key_low === zone.key_high + 1
        : candidate.key_high === zone.key_low - 1
    ));
    return candidates.find((candidate) => candidate.id === selectedZoneId) || candidates.at(-1) || null;
  };

  const updateZoneBlockGeometry = (zone) => {
    const block = builderLane?.querySelector(`[data-zone-id="${zone.id}"]`);
    if (!block) return;
    block.style.top = `calc(var(--builder-row) * ${127 - zone.key_high} + 2px)`;
    block.style.height = `calc(var(--builder-row) * ${zone.key_high - zone.key_low + 1} - 4px)`;
    const detail = block.querySelector(".zone-pill-detail");
    if (detail) {
      detail.textContent = `${noteName(zone.key_low)}–${noteName(zone.key_high)} · ${zone.layers.length} layer${zone.layers.length === 1 ? "" : "s"}${zone.envelope_override ? " · ADSR override" : ""}`;
    }
  };

  const resizeZoneBoundary = (zoneId, edge, requestedNote, coupledZoneId = null) => {
    const zone = builderState.zones.find((candidate) => candidate.id === zoneId);
    if (!zone) return false;
    const coupled = coupledZoneId
      ? builderState.zones.find((candidate) => candidate.id === coupledZoneId)
      : coupledZoneAtEdge(zone, edge);
    const previous = edge === "high" ? zone.key_high : zone.key_low;
    if (edge === "high") {
      const maximum = coupled ? coupled.key_high - 1 : 127;
      zone.key_high = Math.round(clampNumber(requestedNote, zone.key_low, maximum, previous));
      zone.root_key = Math.min(zone.root_key, zone.key_high);
      if (coupled) {
        coupled.key_low = zone.key_high + 1;
        coupled.root_key = Math.max(coupled.root_key, coupled.key_low);
      }
    } else {
      const minimum = coupled ? coupled.key_low + 1 : 0;
      zone.key_low = Math.round(clampNumber(requestedNote, minimum, zone.key_high, previous));
      zone.root_key = Math.max(zone.root_key, zone.key_low);
      if (coupled) {
        coupled.key_high = zone.key_low - 1;
        coupled.root_key = Math.min(coupled.root_key, coupled.key_high);
      }
    }
    updateZoneBlockGeometry(zone);
    if (coupled) updateZoneBlockGeometry(coupled);
    return previous !== (edge === "high" ? zone.key_high : zone.key_low);
  };

  const beginZoneResize = (event, zone, edge) => {
    beginBuilderAudition();
    event.preventDefault();
    event.stopPropagation();
    selectedZoneId = zone.id;
    const coupled = coupledZoneAtEdge(zone, edge);
    let moved = false;
    const move = (pointerEvent) => {
      const rect = builderLane.getBoundingClientRect();
      const note = 127 - Math.floor((pointerEvent.clientY - rect.top) / builderRowHeight);
      moved = resizeZoneBoundary(zone.id, edge, note, coupled?.id) || moved;
    };
    const finish = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", finish);
      window.removeEventListener("pointercancel", finish);
      if (moved) {
        suppressZoneClickUntil = performance.now() + 260;
        persistBuilder();
        renderBuilderZones();
        setBuilderMessage(coupled
          ? `Shared boundary updated: ${zone.name} and ${coupled.name}.`
          : `${zone.name} resized.`, "success");
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", finish, { once: true });
    window.addEventListener("pointercancel", finish, { once: true });
  };

  const createZoneResizeHandle = (zone, edge) => {
    const handle = document.createElement("span");
    handle.className = `zone-resize-handle ${edge}`;
    handle.role = "separator";
    handle.tabIndex = 0;
    handle.setAttribute("aria-orientation", "horizontal");
    handle.setAttribute("aria-label", `Resize ${edge === "high" ? "upper" : "lower"} edge of ${zone.name}`);
    handle.addEventListener("pointerdown", (event) => beginZoneResize(event, zone, edge));
    handle.addEventListener("click", (event) => event.stopPropagation());
    handle.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
      event.preventDefault();
      event.stopPropagation();
      beginBuilderAudition();
      const delta = event.key === "ArrowUp" ? 1 : -1;
      const current = edge === "high" ? zone.key_high : zone.key_low;
      if (resizeZoneBoundary(zone.id, edge, current + delta)) {
        persistBuilder();
        renderBuilderZones();
        builderLane?.querySelector(`[data-zone-id="${zone.id}"] .zone-resize-handle.${edge}`)?.focus();
      }
    });
    return handle;
  };

  const renderBuilderKeys = () => {
    if (!builderKeys) return;
    const fragment = document.createDocumentFragment();
    const black = new Set([1, 3, 6, 8, 10]);
    for (let note = 127; note >= 0; note -= 1) {
      const key = document.createElement("div");
      key.className = `builder-key${black.has(note % 12) ? " black" : ""}${note === 60 ? " middle-c" : ""}`;
      key.style.top = `calc(var(--builder-row) * ${127 - note})`;
      key.dataset.note = String(note);
      const label = document.createElement("span");
      label.textContent = `${noteName(note)}  ${note}`;
      key.append(label);
      key.addEventListener("click", () => openZoneForNote(note));
      fragment.append(key);
    }
    builderKeys.replaceChildren(fragment);
  };

  const renderBuilderZones = () => {
    if (!builderLane) return;
    const fragment = document.createDocumentFragment();
    for (let note = 127; note >= 0; note -= 1) {
      const row = document.createElement("div");
      row.className = "builder-lane-row";
      row.style.top = `calc(var(--builder-row) * ${127 - note})`;
      row.dataset.note = String(note);
      row.title = `Create zone at ${noteName(note)}`;
      row.addEventListener("pointerenter", () => {
        builderKeys?.querySelector(`[data-note="${note}"]`)?.classList.add("hovered");
      });
      row.addEventListener("pointerleave", () => {
        builderKeys?.querySelector(`[data-note="${note}"]`)?.classList.remove("hovered");
      });
      row.addEventListener("click", () => openZoneForNote(note));
      fragment.append(row);
    }
    for (const [index, zone] of builderState.zones.entries()) {
      const pill = document.createElement("div");
      pill.role = "button";
      pill.tabIndex = 0;
      pill.className = `zone-pill${zone.id === selectedZoneId ? " selected" : ""}`;
      pill.style.top = `calc(var(--builder-row) * ${127 - zone.key_high} + 2px)`;
      pill.style.height = `calc(var(--builder-row) * ${zone.key_high - zone.key_low + 1} - 4px)`;
      pill.style.setProperty("--zone-rgb", zoneColorRgb(zone.color));
      pill.dataset.zoneId = zone.id;
      const title = document.createElement("strong");
      title.textContent = zone.name || `Zone ${index + 1}`;
      const detail = document.createElement("span");
      detail.className = "zone-pill-detail";
      detail.textContent = `${noteName(zone.key_low)}–${noteName(zone.key_high)} · ${zone.layers.length} layer${zone.layers.length === 1 ? "" : "s"}${zone.envelope_override ? " · ADSR override" : ""}`;
      pill.append(createZoneResizeHandle(zone, "high"), title, detail, createZoneResizeHandle(zone, "low"));
      pill.addEventListener("click", (event) => {
        event.stopPropagation();
        if (performance.now() < suppressZoneClickUntil) return;
        selectedZoneId = zone.id;
        openZoneEditor(zone);
      });
      pill.addEventListener("keydown", (event) => {
        if (event.target !== pill || (event.key !== "Enter" && event.key !== " ")) return;
        event.preventDefault();
        selectedZoneId = zone.id;
        openZoneEditor(zone);
      });
      fragment.append(pill);
    }
    builderLane.replaceChildren(fragment);
    if (builderSummary) {
      const layers = builderState.zones.reduce((total, zone) => total + zone.layers.length, 0);
      builderSummary.textContent = builderState.zones.length
        ? `${builderState.zones.length} zone${builderState.zones.length === 1 ? "" : "s"} · ${layers} velocity layer${layers === 1 ? "" : "s"}`
        : "No zones yet";
    }
  };

  const updateBuilderZoom = (height) => {
    builderRowHeight = clampNumber(height, 18, 72, 28);
    builderViewport?.style.setProperty("--builder-row", `${builderRowHeight}px`);
  };

  builderViewport?.addEventListener("wheel", (event) => {
    if (!event.ctrlKey) return;
    event.preventDefault();
    const rect = builderViewport.getBoundingClientRect();
    const pointerY = event.clientY - rect.top;
    const pitchPosition = (builderViewport.scrollTop + pointerY) / builderRowHeight;
    updateBuilderZoom(builderRowHeight + (event.deltaY < 0 ? 4 : -4));
    builderViewport.scrollTop = pitchPosition * builderRowHeight - pointerY;
  }, { passive: false });

  const renderLayerList = () => {
    if (!layerList || !editingZone) return;
    const fragment = document.createDocumentFragment();
    if (!editingZone.layers.length) {
      const empty = document.createElement("div");
      empty.className = "layer-empty";
      const title = document.createElement("strong");
      title.textContent = "0 layers";
      const detail = document.createElement("span");
      detail.textContent = "Add a layer to load a WAV or FLAC sample and define its velocity response.";
      empty.append(title, detail);
      fragment.append(empty);
    }
    for (const [layerIndex, layer] of editingZone.layers.entries()) {
      const row = document.createElement("div");
      row.className = "layer-row";
      const layerTitle = document.createElement("div");
      layerTitle.className = "layer-card-title";
      layerTitle.textContent = `Layer ${String(layerIndex + 1).padStart(2, "0")}`;
      row.append(layerTitle);
      const sampleField = document.createElement("label");
      sampleField.className = "layer-field sample-field";
      const sampleCaption = document.createElement("span");
      sampleCaption.textContent = "Sample";
      const sampleButton = document.createElement("button");
      sampleButton.type = "button";
      sampleButton.className = "sample-pick";
      sampleButton.textContent = layer.sample_name || "Load sample";
      sampleButton.title = layer.sample_name || "Choose sample";
      sampleButton.addEventListener("click", () => {
        sampleTargetLayerId = layer.id;
        builderSamples?.click();
      });
      sampleField.append(sampleCaption, sampleButton);
      row.append(sampleField);
      const numericFields = [
        ["velocity_low", "Velocity low", 1, 127, 1],
        ["velocity_high", "Velocity high", 1, 127, 1],
        ["volume_percent", "Volume %", 0, MAX_VOLUME_PERCENT, 0.01],
        ["pan", "Pan", -1, 1, 0.01],
        ["tune_semitones", "Tune", -48, 48, 0.01],
      ];
      for (const [key, captionText, min, max, step] of numericFields) {
        const field = document.createElement("label");
        field.className = "layer-field";
        const caption = document.createElement("span");
        caption.textContent = captionText;
        const input = document.createElement("input");
        input.type = "number";
        input.min = min;
        input.max = max;
        input.step = step;
        input.value = layer[key];
      input.addEventListener("input", () => {
        layer[key] = Number(input.value);
        scheduleBuilderPreview();
      });
        field.append(caption, input);
        row.append(field);
      }
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "layer-remove";
      remove.textContent = "×";
      remove.title = "Remove layer";
      remove.addEventListener("click", () => {
        editingZone.layers = editingZone.layers.filter((candidate) => candidate.id !== layer.id);
        builderFiles.delete(layer.id);
        builderWaveforms.delete(layer.id);
        renderLayerList();
        scheduleBuilderPreview();
      });
      row.append(remove);
      row.append(createLayerWaveform(layer));
      fragment.append(row);
    }
    layerList.replaceChildren(fragment);
  };

  const zoneField = (selector) => zoneDialog?.querySelector(selector);

  const openZoneEditor = (zone) => {
    if (!zoneDialog) return;
    beginBuilderAudition();
    editingZone = structuredClone(zone);
    selectedZoneId = zone.id;
    for (const key of builderKeys?.querySelectorAll(".hovered") ?? []) {
      key.classList.remove("hovered");
    }
    renderBuilderZones();
    zoneField("[data-zone-name]").value = editingZone.name;
    zoneField("[data-zone-low]").value = editingZone.key_low;
    zoneField("[data-zone-high]").value = editingZone.key_high;
    zoneField("[data-zone-root]").value = editingZone.root_key;
    const color = zoneField("[data-zone-color]");
    color.value = normalizeZoneColor(editingZone.color);
    zoneField("[data-zone-color-output]").value = color.value.toUpperCase();
    zoneDialog.style.setProperty("--editing-zone-rgb", zoneColorRgb(color.value));
    const override = zoneField("[data-zone-envelope-override]");
    override.checked = Boolean(editingZone.envelope_override);
    if (!editingZone.envelope_override) editingZone.envelope_override = structuredClone(builderState.envelope);
    zoneEnvelopeHost?.setAttribute("aria-disabled", String(!override.checked));
    renderEnvelope(zoneEnvelopeHost, editingZone.envelope_override, scheduleBuilderPreview);
    renderLayerList();
    for (const layer of editingZone.layers) {
      const file = builderFiles.get(layer.id);
      if (file && !builderWaveforms.has(layer.id)) analyzeWaveform(layer.id, file);
    }
    zoneDialog.showModal();
    zoneField("[data-zone-name]").focus();
  };

  const closeZoneEditor = () => {
    editingZone = null;
    if (zoneDialog?.open) zoneDialog.close();
    scheduleBuilderPreview();
  };

  zoneField("[data-zone-envelope-override]")?.addEventListener("change", (event) => {
    zoneEnvelopeHost?.setAttribute("aria-disabled", String(!event.target.checked));
    scheduleBuilderPreview();
  });
  zoneField("[data-zone-color]")?.addEventListener("input", (event) => {
    if (!editingZone) return;
    editingZone.color = normalizeZoneColor(event.target.value);
    zoneField("[data-zone-color-output]").value = editingZone.color.toUpperCase();
    zoneDialog.style.setProperty("--editing-zone-rgb", zoneColorRgb(editingZone.color));
    scheduleBuilderPreview();
  });
  const zoneColorPresets = zoneField("[data-zone-color-presets]");
  for (const color of ZONE_COLORS) {
    const swatch = document.createElement("button");
    swatch.type = "button";
    swatch.className = "zone-color-swatch";
    swatch.style.setProperty("--swatch", color);
    swatch.title = color.toUpperCase();
    swatch.setAttribute("aria-label", `Use zone color ${color.toUpperCase()}`);
    swatch.addEventListener("click", () => {
      const input = zoneField("[data-zone-color]");
      input.value = color;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    zoneColorPresets?.append(swatch);
  }
  zoneField("[data-zone-close]")?.addEventListener("click", closeZoneEditor);
  zoneField("[data-zone-cancel]")?.addEventListener("click", closeZoneEditor);
  zoneDialog?.addEventListener("cancel", () => {
    editingZone = null;
    scheduleBuilderPreview();
  });
  zoneField("[data-layer-add]")?.addEventListener("click", () => {
    if (!editingZone) return;
    const splittable = [...editingZone.layers]
      .filter((layer) => Number(layer.velocity_high) > Number(layer.velocity_low))
      .sort((left, right) =>
        (right.velocity_high - right.velocity_low) - (left.velocity_high - left.velocity_low))[0];
    if (splittable) {
      const previousHigh = Number(splittable.velocity_high);
      const midpoint = Math.floor((Number(splittable.velocity_low) + previousHigh) / 2);
      splittable.velocity_high = midpoint;
      editingZone.layers.push(newLayer(midpoint + 1, previousHigh));
    } else {
      editingZone.layers.push(newLayer());
    }
    renderLayerList();
    scheduleBuilderPreview();
  });

  builderSamples?.addEventListener("change", () => {
    if (!editingZone || !sampleTargetLayerId) return;
    const files = [...builderSamples.files];
    if (!files.length) return;
    const accepted = files.filter((file) => /\.(wav|wave|flac)$/i.test(file.name));
    if (accepted.length !== files.length) {
      setBuilderMessage("Only WAV, WAVE and FLAC samples are supported in the instrument builder.", "error");
    }
    const targetIndex = editingZone.layers.findIndex((layer) => layer.id === sampleTargetLayerId);
    accepted.forEach((file, index) => {
      let layer = editingZone.layers[targetIndex + index];
      if (!layer) {
        layer = newLayer();
        editingZone.layers.push(layer);
      }
      layer.sample_name = file.name;
      layer.sample_path = "";
      builderFiles.set(layer.id, file);
      builderWaveforms.delete(layer.id);
      analyzeWaveform(layer.id, file);
    });
    builderSamples.value = "";
    sampleTargetLayerId = null;
    renderLayerList();
    scheduleBuilderPreview();
  });

  for (const selector of ["[data-zone-name]", "[data-zone-low]", "[data-zone-high]", "[data-zone-root]"]) {
    zoneField(selector)?.addEventListener("input", scheduleBuilderPreview);
  }

  const readZoneFields = () => {
    if (!editingZone) return null;
    editingZone.name = zoneField("[data-zone-name]").value.trim() || "Untitled zone";
    editingZone.key_low = Number(zoneField("[data-zone-low]").value);
    editingZone.key_high = Number(zoneField("[data-zone-high]").value);
    editingZone.root_key = Number(zoneField("[data-zone-root]").value);
    editingZone.color = normalizeZoneColor(zoneField("[data-zone-color]").value);
    if (!zoneField("[data-zone-envelope-override]").checked) editingZone.envelope_override = null;
    return editingZone;
  };

  const validateZone = (zone, requireLayers = false) => {
    if (!Number.isInteger(zone.key_low) || !Number.isInteger(zone.key_high)
        || !Number.isInteger(zone.root_key) || zone.key_low < 0 || zone.key_high > 127
        || zone.key_low > zone.key_high || zone.root_key < zone.key_low || zone.root_key > zone.key_high) {
      return "The zone key range is invalid, or the root key is outside it.";
    }
    if (requireLayers && !zone.layers.length) return "The zone needs at least one velocity layer.";
    for (const layer of zone.layers) {
      if (!Number.isInteger(layer.velocity_low) || !Number.isInteger(layer.velocity_high)
          || layer.velocity_low < 1 || layer.velocity_high > 127
          || layer.velocity_low > layer.velocity_high) {
        return `Velocity range is invalid in ${layer.sample_name || "an unassigned layer"}.`;
      }
      if (!Number.isFinite(layer.volume_percent) || layer.volume_percent < 0 || layer.volume_percent > MAX_VOLUME_PERCENT
          || !Number.isFinite(layer.pan) || layer.pan < -1 || layer.pan > 1
          || !Number.isFinite(layer.tune_semitones) || layer.tune_semitones < -48 || layer.tune_semitones > 48) {
        return `Volume, pan or tuning is invalid in ${layer.sample_name || "an unassigned layer"}.`;
      }
    }
    return null;
  };

  zoneField("[data-zone-save]")?.addEventListener("click", () => {
    const zone = readZoneFields();
    const error = zone && validateZone(zone, false);
    if (error) {
      setBuilderMessage(error, "error");
      return;
    }
    const index = builderState.zones.findIndex((candidate) => candidate.id === zone.id);
    if (index >= 0) builderState.zones[index] = zone;
    else builderState.zones.push(zone);
    selectedZoneId = zone.id;
    persistBuilder();
    renderBuilderZones();
    setBuilderMessage(`Saved ${zone.name}.`, "success");
    closeZoneEditor();
  });

  zoneField("[data-zone-delete]")?.addEventListener("click", () => {
    if (!editingZone) return;
    const known = builderState.zones.some((zone) => zone.id === editingZone.id);
    if (known) {
      for (const layer of editingZone.layers) {
        builderFiles.delete(layer.id);
        builderWaveforms.delete(layer.id);
      }
      builderState.zones = builderState.zones.filter((zone) => zone.id !== editingZone.id);
      selectedZoneId = null;
      persistBuilder();
      renderBuilderZones();
      setBuilderMessage("Zone deleted.");
    }
    closeZoneEditor();
  });

  builderNewZone?.addEventListener("click", () => {
    beginBuilderAudition();
    openZoneEditor(newZone(60));
  });
  const consumeRfBankFile = async (file, handle = null) => {
    builderOpen.disabled = true;
    builderExport.disabled = true;
    setBuilderMessage(`Opening ${file.name} and validating its contents…`);
    try {
      await openRfBankFile(file, handle);
    } catch (error) {
      setBuilderMessage(error instanceof Error ? error.message : "The RF Bank could not be opened.", "error");
    } finally {
      builderOpen.disabled = false;
      builderExport.disabled = false;
    }
  };
  if (builderOpen) builderOpen.title = "Open an RF Bank · Hold Shift to use the portable file selector";
  builderOpen?.addEventListener("click", async (event) => {
    if (event.shiftKey || typeof window.showOpenFilePicker !== "function") {
      builderOpenFile?.click();
      return;
    }
    try {
      const [handle] = await window.showOpenFilePicker({
        multiple: false,
        types: [{
          description: "RF Instrument Bank",
          accept: { "application/vnd.rackforge.bank+zip": [".rfbank"] },
        }],
      });
      if (handle) await consumeRfBankFile(await handle.getFile(), handle);
    } catch (error) {
      if (error?.name === "AbortError") return;
      builderOpenFile?.click();
    }
  });
  builderOpenFile?.addEventListener("change", () => {
    const file = builderOpenFile.files?.[0];
    builderOpenFile.value = "";
    if (file) consumeRfBankFile(file);
  });
  builderInstrument?.addEventListener("change", () => activateOpenedInstrument(Number(builderInstrument.value)));
  let newBlankResetTimer = 0;
  builderNewBank?.addEventListener("click", () => {
    if (!builderNewBank.hasAttribute("data-confirming")) {
      builderNewBank.setAttribute("data-confirming", "");
      builderNewBank.textContent = "Confirm new blank";
      window.clearTimeout(newBlankResetTimer);
      newBlankResetTimer = window.setTimeout(() => {
        builderNewBank.removeAttribute("data-confirming");
        builderNewBank.textContent = "New blank";
      }, 4_000);
      return;
    }
    window.clearTimeout(newBlankResetTimer);
    builderNewBank.removeAttribute("data-confirming");
    builderNewBank.textContent = "New blank";
    resetBuilderToBlank();
  });
  builderName?.addEventListener("input", () => {
    beginBuilderAudition();
    builderState.name = builderName.value;
    persistBuilder();
  });

  const slugFile = (value) => {
    const folded = value.normalize("NFKD").replace(/[\u0300-\u036f]/g, "")
      .toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
    return folded || "rf-instrument";
  };

  const crcTable = (() => {
    const table = new Uint32Array(256);
    for (let index = 0; index < 256; index += 1) {
      let value = index;
      for (let bit = 0; bit < 8; bit += 1) value = (value & 1) ? (0xedb88320 ^ (value >>> 1)) : (value >>> 1);
      table[index] = value >>> 0;
    }
    return table;
  })();

  const crc32 = (bytes) => {
    let value = 0xffffffff;
    for (const byte of bytes) value = crcTable[(value ^ byte) & 0xff] ^ (value >>> 8);
    return (value ^ 0xffffffff) >>> 0;
  };

  const zipError = (message) => new Error(`RF Bank: ${message}`);

  const normalizeZipPath = (value) => {
    const path = String(value || "").replaceAll("\\", "/");
    if (!path || path.length > 1_024 || path.includes("\0") || path.startsWith("/")
        || /^[a-z]:/i.test(path) || path.split("/").some((part) => part === "." || part === "..")) {
      throw zipError(`unsafe entry path ${JSON.stringify(value)}`);
    }
    return path;
  };

  const findZipEnd = (bytes) => {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const earliest = Math.max(0, bytes.length - 65_557);
    for (let offset = bytes.length - 22; offset >= earliest; offset -= 1) {
      if (view.getUint32(offset, true) !== 0x06054b50) continue;
      const commentLength = view.getUint16(offset + 20, true);
      if (offset + 22 + commentLength === bytes.length) return offset;
    }
    throw zipError("the archive directory is missing or truncated");
  };

  const indexZip = async (file) => {
    if (!file || file.size < 22) throw zipError("the selected file is empty or truncated");
    if (file.size > MAX_BANK_BYTES) throw zipError("the selected file exceeds the 128 MB bank limit");
    const bytes = new Uint8Array(await file.arrayBuffer());
    const view = new DataView(bytes.buffer);
    const endOffset = findZipEnd(bytes);
    const disk = view.getUint16(endOffset + 4, true);
    const directoryDisk = view.getUint16(endOffset + 6, true);
    const entriesOnDisk = view.getUint16(endOffset + 8, true);
    const entryCount = view.getUint16(endOffset + 10, true);
    const directorySize = view.getUint32(endOffset + 12, true);
    const directoryOffset = view.getUint32(endOffset + 16, true);
    if (disk !== 0 || directoryDisk !== 0 || entriesOnDisk !== entryCount) {
      throw zipError("multi-volume archives are not supported");
    }
    if (entryCount === 0 || entryCount > MAX_BANK_ENTRIES) {
      throw zipError("the archive has an invalid number of entries");
    }
    if (entryCount === 0xffff || directorySize === 0xffffffff || directoryOffset === 0xffffffff) {
      throw zipError("ZIP64 banks are not supported within the 128 MB RF Bank limit");
    }
    const directoryEnd = directoryOffset + directorySize;
    if (directoryOffset > endOffset || directoryEnd > endOffset) {
      throw zipError("the archive directory points outside the file");
    }
    const decoder = new TextDecoder("utf-8");
    const entries = [];
    const names = new Set();
    let totalExpanded = 0;
    let cursor = directoryOffset;
    for (let index = 0; index < entryCount; index += 1) {
      if (cursor + 46 > directoryEnd || view.getUint32(cursor, true) !== 0x02014b50) {
        throw zipError(`central directory entry ${index + 1} is invalid`);
      }
      const flags = view.getUint16(cursor + 8, true);
      const method = view.getUint16(cursor + 10, true);
      const checksum = view.getUint32(cursor + 16, true);
      const compressedSize = view.getUint32(cursor + 20, true);
      const size = view.getUint32(cursor + 24, true);
      const nameLength = view.getUint16(cursor + 28, true);
      const extraLength = view.getUint16(cursor + 30, true);
      const commentLength = view.getUint16(cursor + 32, true);
      const entryDisk = view.getUint16(cursor + 34, true);
      const localOffset = view.getUint32(cursor + 42, true);
      const recordEnd = cursor + 46 + nameLength + extraLength + commentLength;
      if (recordEnd > directoryEnd || localOffset + 30 > directoryOffset) {
        throw zipError(`central directory entry ${index + 1} is truncated`);
      }
      const name = normalizeZipPath(decoder.decode(bytes.subarray(cursor + 46, cursor + 46 + nameLength)));
      const foldedName = name.toLocaleLowerCase("en-US");
      if (entryDisk !== 0) throw zipError(`${name} belongs to another archive volume`);
      if (flags & 0x0001) throw zipError(`${name} is encrypted`);
      if (method !== 0 && method !== 8) throw zipError(`${name} uses unsupported ZIP compression method ${method}`);
      if (compressedSize > MAX_BANK_BYTES || size > MAX_BANK_BYTES) throw zipError(`${name} is too large`);
      totalExpanded += size;
      if (totalExpanded > MAX_BANK_BYTES) throw zipError("expanded bank content exceeds 128 MB");
      if (!name.endsWith("/")) {
        if (names.has(foldedName)) throw zipError(`duplicate entry path ${name}`);
        names.add(foldedName);
        entries.push({ name, foldedName, flags, method, checksum, compressedSize, size, localOffset });
      }
      cursor = recordEnd;
    }
    if (cursor !== directoryEnd) throw zipError("the archive directory size is inconsistent");
    return { bytes, entries, entryByName: new Map(entries.map((entry) => [entry.foldedName, entry])) };
  };

  const extractZipEntry = async (archive, entry) => {
    if (entry.data) return entry.data;
    const view = new DataView(archive.bytes.buffer, archive.bytes.byteOffset, archive.bytes.byteLength);
    if (view.getUint32(entry.localOffset, true) !== 0x04034b50) {
      throw zipError(`${entry.name} has an invalid local header`);
    }
    const nameLength = view.getUint16(entry.localOffset + 26, true);
    const extraLength = view.getUint16(entry.localOffset + 28, true);
    const start = entry.localOffset + 30 + nameLength + extraLength;
    const end = start + entry.compressedSize;
    if (start > archive.bytes.length || end > archive.bytes.length) {
      throw zipError(`${entry.name} data is truncated`);
    }
    const compressed = archive.bytes.subarray(start, end);
    let data;
    if (entry.method === 0) {
      data = compressed.slice();
    } else {
      if (typeof DecompressionStream !== "function") {
        throw zipError(`${entry.name} is compressed and this host cannot expand it`);
      }
      try {
        const stream = new Blob([compressed]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
        data = new Uint8Array(await new Response(stream).arrayBuffer());
      } catch (_) {
        throw zipError(`${entry.name} could not be decompressed`);
      }
    }
    if (data.length !== entry.size) throw zipError(`${entry.name} expanded to the wrong size`);
    if (crc32(data) !== entry.checksum) throw zipError(`${entry.name} failed its checksum`);
    entry.data = data;
    return data;
  };

  const readZipJson = async (archive, entry, label) => {
    if (entry.size > MAX_MAP_BYTES) throw zipError(`${label} exceeds the 2 MB map limit`);
    let value;
    try {
      value = JSON.parse(new TextDecoder().decode(await extractZipEntry(archive, entry)));
    } catch (error) {
      if (error instanceof Error && error.message.startsWith("RF Bank:")) throw error;
      throw zipError(`${label} is not valid JSON`);
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) throw zipError(`${label} must be a JSON object`);
    return value;
  };

  const jsonExtras = (value, known) => Object.fromEntries(
    Object.entries(value).filter(([key]) => !known.has(key)),
  );

  const importEnvelope = (value, label) => {
    if (!value || typeof value !== "object" || Array.isArray(value)) throw zipError(`${label} is missing`);
    const envelope = {
      attack: Number(value.attack_seconds ?? 0),
      decay: Number(value.decay_seconds ?? .15),
      sustain: Number(value.sustain_level ?? 1),
      release: Number(value.release_seconds ?? .35),
      _rf_extra: jsonExtras(value, new Set(["attack_seconds", "decay_seconds", "sustain_level", "release_seconds"])),
    };
    if (!Number.isFinite(envelope.attack) || !Number.isFinite(envelope.decay)
        || !Number.isFinite(envelope.sustain) || !Number.isFinite(envelope.release)
        || envelope.attack < 0 || envelope.attack > 30 || envelope.decay < 0 || envelope.decay > 30
        || envelope.sustain < 0 || envelope.sustain > 1 || envelope.release < 0 || envelope.release > 30) {
      throw zipError(`${label} has values outside the supported range`);
    }
    return envelope;
  };

  const sampleMime = (name) => name.toLowerCase().endsWith(".flac") ? "audio/flac" : "audio/wav";

  const importRfInstrument = async (archive, entry, instrumentIndex, sampleEntries) => {
    const value = await readZipJson(archive, entry, entry.name);
    if (Number(value.schema_version ?? 0) > 1) throw zipError(`${entry.name} uses a newer instrument schema`);
    if (!Array.isArray(value.zones) || !value.zones.length) throw zipError(`${entry.name} has no key zones`);
    const state = {
      schema_version: 1,
      name: String(value.name || entry.name.split("/").at(-1).replace(/\.rfinstrument$/i, "")).trim() || `RF Instrument ${instrumentIndex + 1}`,
      envelope: importEnvelope(value.envelope, `${entry.name} envelope`),
      zones: [],
      _rf_extra: jsonExtras(value, new Set(["schema_version", "id", "name", "envelope", "zones"])),
      _rf_id: String(value.id || "").trim(),
    };
    const files = new Map();
    let instrumentSampleBytes = 0;
    const countedSamples = new Set();
    for (const [zoneIndex, sourceZone] of value.zones.entries()) {
      const label = `${entry.name} zone ${zoneIndex + 1}`;
      if (!sourceZone || typeof sourceZone !== "object" || Array.isArray(sourceZone)) throw zipError(`${label} is not an object`);
      const keyLow = Number(sourceZone.key_low);
      const keyHigh = Number(sourceZone.key_high);
      const rootKey = Number(sourceZone.root_key);
      if (!Number.isInteger(keyLow) || !Number.isInteger(keyHigh) || !Number.isInteger(rootKey)
          || keyLow < 0 || keyHigh > 127 || keyLow > keyHigh || rootKey < keyLow || rootKey > keyHigh) {
        throw zipError(`${label} has an invalid key range`);
      }
      if (!Array.isArray(sourceZone.layers) || !sourceZone.layers.length) throw zipError(`${label} has no velocity layers`);
      const zone = {
        id: builderId("zone"),
        name: String(sourceZone.name || `Zone ${noteName(rootKey)}`).trim() || `Zone ${zoneIndex + 1}`,
        color: normalizeZoneColor(sourceZone.color, ZONE_COLORS[zoneIndex % ZONE_COLORS.length]),
        key_low: keyLow,
        key_high: keyHigh,
        root_key: rootKey,
        envelope_override: sourceZone.envelope_override
          ? importEnvelope(sourceZone.envelope_override, `${label} envelope`) : null,
        layers: [],
        _rf_extra: jsonExtras(sourceZone, new Set(["name", "color", "key_low", "key_high", "root_key", "envelope_override", "layers"])),
      };
      for (const [layerIndex, sourceLayer] of sourceZone.layers.entries()) {
        if (!sourceLayer || typeof sourceLayer !== "object" || Array.isArray(sourceLayer)) {
          throw zipError(`${label} layer ${layerIndex + 1} is not an object`);
        }
        const sampleReference = typeof sourceLayer.sample === "string" ? sourceLayer.sample.trim() : "";
        const sampleBase = sampleReference.replaceAll("\\", "/").split("/").at(-1).toLocaleLowerCase("en-US");
        const sampleEntry = sampleEntries.get(sampleBase);
        const velocityLow = Number(sourceLayer.velocity_low ?? 1);
        const velocityHigh = Number(sourceLayer.velocity_high ?? 127);
        const gainDb = Number(sourceLayer.gain_db ?? 0);
        const pan = Number(sourceLayer.pan ?? 0);
        const tune = Number(sourceLayer.tune_semitones ?? 0);
        if (!sampleReference || !sampleEntry) throw zipError(`${label} layer ${layerIndex + 1} references missing sample ${sampleReference || "(empty)"}`);
        if (!Number.isInteger(velocityLow) || !Number.isInteger(velocityHigh) || velocityLow < 1
            || velocityHigh > 127 || velocityLow > velocityHigh || !Number.isFinite(gainDb)
            || gainDb < -60 || gainDb > 24 || !Number.isFinite(pan) || pan < -1 || pan > 1
            || !Number.isFinite(tune) || tune < -48 || tune > 48) {
          throw zipError(`${label} layer ${layerIndex + 1} has invalid playback values`);
        }
        if (!countedSamples.has(sampleEntry.foldedName)) {
          countedSamples.add(sampleEntry.foldedName);
          instrumentSampleBytes += sampleEntry.size;
        }
        if (instrumentSampleBytes > SAMPLE_LIMIT_BYTES) throw zipError(`${entry.name} exceeds the 160 MB selected-instrument sample limit`);
        const layer = {
          id: builderId("layer"),
          sample_name: sampleEntry.name.split("/").at(-1),
          sample_path: sampleEntry.name,
          velocity_low: velocityLow,
          velocity_high: velocityHigh,
          volume_percent: Number((100 * (10 ** (gainDb / 20))).toFixed(4)),
          pan,
          tune_semitones: tune,
          _rf_extra: jsonExtras(sourceLayer, new Set(["sample", "velocity_low", "velocity_high", "gain_db", "pan", "tune_semitones"])),
        };
        let file = archive.sampleFiles.get(sampleEntry.foldedName);
        if (!file) {
          file = new File([await extractZipEntry(archive, sampleEntry)], layer.sample_name, {
            type: sampleMime(layer.sample_name),
            lastModified: 0,
          });
          archive.sampleFiles.set(sampleEntry.foldedName, file);
        }
        files.set(layer.id, file);
        zone.layers.push(layer);
      }
      state.zones.push(zone);
    }
    return { path: entry.name, foldedPath: entry.foldedName, state, files };
  };

  const syncOpenedInstrument = () => {
    if (!openedBank || openedBank.currentIndex < 0) return;
    const instrument = openedBank.instruments[openedBank.currentIndex];
    instrument.state = structuredClone(builderState);
    instrument.files = new Map(builderFiles);
  };

  const renderOpenedBankContext = () => {
    const active = Boolean(openedBank);
    if (builderBankContext) builderBankContext.hidden = !active;
    if (builderSave) builderSave.hidden = !openedBank?.handle?.createWritable;
    if (builderExport) builderExport.textContent = active ? "Save As" : "Export RF Bank";
    if (!active) return;
    builderBankName.textContent = openedBank.fileName;
    const legacy = openedBank.legacyCount;
    builderBankDetail.textContent = `${openedBank.instruments.length} editable RF instrument${openedBank.instruments.length === 1 ? "" : "s"}${legacy ? ` · ${legacy} inherited map${legacy === 1 ? "" : "s"} preserved read-only` : ""}`;
    const fragment = document.createDocumentFragment();
    for (const [index, instrument] of openedBank.instruments.entries()) {
      const option = document.createElement("option");
      option.value = String(index);
      option.textContent = instrument.state.name;
      fragment.append(option);
    }
    builderInstrument.replaceChildren(fragment);
    builderInstrument.value = String(openedBank.currentIndex);
  };

  const activateOpenedInstrument = (index) => {
    if (!openedBank || !openedBank.instruments[index]) return;
    syncOpenedInstrument();
    openedBank.currentIndex = index;
    const instrument = openedBank.instruments[index];
    beginBuilderAudition();
    builderState = structuredClone(instrument.state);
    builderFiles.clear();
    for (const [layerId, file] of instrument.files) builderFiles.set(layerId, file);
    builderWaveforms.clear();
    selectedZoneId = null;
    editingZone = null;
    builderName.value = builderState.name;
    renderEnvelope(globalEnvelopeHost, builderState.envelope, () => {
      beginBuilderAudition();
      persistBuilder();
    });
    renderBuilderZones();
    renderOpenedBankContext();
    persistBuilder();
    setBuilderMessage(`Editing ${builderState.name} from ${openedBank.fileName}.`, "success");
  };

  const resetBuilderToBlank = () => {
    beginBuilderAudition();
    openedBank = null;
    builderState = defaultBuilderState();
    builderFiles.clear();
    builderWaveforms.clear();
    selectedZoneId = null;
    editingZone = null;
    builderName.value = builderState.name;
    renderEnvelope(globalEnvelopeHost, builderState.envelope, () => {
      beginBuilderAudition();
      persistBuilder();
    });
    renderBuilderZones();
    renderOpenedBankContext();
    persistBuilder();
    setBuilderMessage("Blank RF instrument ready.", "success");
  };

  const openRfBankFile = async (file, handle = null) => {
    if (!file?.name?.toLowerCase().endsWith(".rfbank")) throw zipError("choose a file ending in .rfbank");
    const archive = await indexZip(file);
    archive.sampleFiles = new Map();
    const extensionOf = (name) => name.split("/").at(-1).split(".").at(-1).toLowerCase();
    const basenameOf = (name) => name.split("/").at(-1);
    const sampleEntries = new Map();
    const instrumentEntries = [];
    let manifestEntry = null;
    let legacyCount = 0;
    for (const entry of archive.entries) {
      const extension = extensionOf(entry.name);
      if (!manifestEntry && basenameOf(entry.name).toLowerCase() === "bank.json") manifestEntry = entry;
      if (extension === "rfinstrument") instrumentEntries.push(entry);
      if (extension === "nki") legacyCount += 1;
      if (extension === "wav" || extension === "wave" || extension === "flac") {
        const basename = basenameOf(entry.name).toLocaleLowerCase("en-US");
        const existing = sampleEntries.get(basename);
        const depth = entry.name.split("/").length;
        if (!existing || depth < existing.depth) sampleEntries.set(basename, { ...entry, depth });
      }
    }
    if (!instrumentEntries.length) {
      throw zipError(legacyCount
        ? `this bank contains ${legacyCount} inherited instrument map${legacyCount === 1 ? "" : "s"}, but no editable .rfinstrument maps`
        : "this bank contains no editable .rfinstrument maps");
    }
    const manifest = manifestEntry ? await readZipJson(archive, manifestEntry, "bank.json") : {};
    if (Number(manifest.schema_version ?? 0) > 1) throw zipError("bank.json uses a newer schema");
    instrumentEntries.sort((left, right) => left.name.localeCompare(right.name));
    const instruments = [];
    for (const [index, entry] of instrumentEntries.entries()) {
      instruments.push(await importRfInstrument(archive, entry, index, sampleEntries));
    }
    openedBank = {
      ...archive,
      fileName: file.name,
      handle,
      manifest,
      manifestPath: manifestEntry?.name || "bank.json",
      instruments,
      legacyCount,
      currentIndex: -1,
    };
    activateOpenedInstrument(0);
  };

  const zipRecord = (length, writer) => {
    const bytes = new Uint8Array(length);
    writer(new DataView(bytes.buffer));
    return bytes;
  };

  const makeZip = (entries) => {
    const encoder = new TextEncoder();
    const localParts = [];
    const centralParts = [];
    let offset = 0;
    for (const entry of entries) {
      const name = encoder.encode(entry.name);
      const data = entry.data instanceof Uint8Array ? entry.data : new Uint8Array(entry.data);
      const checksum = crc32(data);
      const local = zipRecord(30, (view) => {
        view.setUint32(0, 0x04034b50, true);
        view.setUint16(4, 20, true);
        view.setUint16(6, 0x0800, true);
        view.setUint16(8, 0, true);
        view.setUint32(14, checksum, true);
        view.setUint32(18, data.length, true);
        view.setUint32(22, data.length, true);
        view.setUint16(26, name.length, true);
      });
      const localOffset = offset;
      localParts.push(local, name, data);
      offset += local.length + name.length + data.length;
      const central = zipRecord(46, (view) => {
        view.setUint32(0, 0x02014b50, true);
        view.setUint16(4, 20, true);
        view.setUint16(6, 20, true);
        view.setUint16(8, 0x0800, true);
        view.setUint16(10, 0, true);
        view.setUint32(16, checksum, true);
        view.setUint32(20, data.length, true);
        view.setUint32(24, data.length, true);
        view.setUint16(28, name.length, true);
        view.setUint32(42, localOffset, true);
      });
      centralParts.push(central, name);
    }
    const centralSize = centralParts.reduce((total, part) => total + part.length, 0);
    const end = zipRecord(22, (view) => {
      view.setUint32(0, 0x06054b50, true);
      view.setUint16(8, entries.length, true);
      view.setUint16(10, entries.length, true);
      view.setUint32(12, centralSize, true);
      view.setUint32(16, offset, true);
    });
    return new Blob([...localParts, ...centralParts, end], { type: "application/vnd.rackforge.bank+zip" });
  };

  const validateBuilderState = (state, filesMap) => {
    const name = state.name.trim();
    if (!name) return { error: "Give the instrument a name." };
    if (!state.zones.length) return { error: "Add at least one key zone." };
    const files = [];
    let totalBytes = 0;
    const countedFiles = new Set();
    for (const zone of state.zones) {
      const zoneError = validateZone(zone, true);
      if (zoneError) return { error: `${zone.name}: ${zoneError}` };
      for (const layer of zone.layers) {
        const file = filesMap.get(layer.id);
        if (!file) return { error: `${zone.name}: choose the sample for ${layer.sample_name || "every velocity layer"}.` };
        if (!countedFiles.has(file)) {
          countedFiles.add(file);
          totalBytes += file.size;
        }
        files.push([layer, file]);
      }
    }
    if (totalBytes > SAMPLE_LIMIT_BYTES) {
      return { error: "This instrument exceeds the 160 MB decoded-sample limit." };
    }
    return { name, files };
  };

  const envelopeForExport = (envelope) => ({
    ...(envelope._rf_extra || {}),
    attack_seconds: envelope.attack,
    decay_seconds: envelope.decay,
    sustain_level: envelope.sustain,
    release_seconds: envelope.release,
  });

  const setBuilderAudition = (state, title, detail) => {
    if (!builderAudition) return;
    builderAudition.dataset.state = state;
    builderAudition.dataset.editMode = String(builderEditMode);
    builderAudition.querySelector("strong").textContent = title;
    builderAudition.querySelector("small").textContent = detail;
  };

  const auditionBuilderState = () => {
    const state = structuredClone(builderState);
    if (!editingZone) return state;
    const zone = structuredClone(editingZone);
    zone.name = zoneField("[data-zone-name]")?.value.trim() || "Untitled zone";
    zone.key_low = Number(zoneField("[data-zone-low]")?.value);
    zone.key_high = Number(zoneField("[data-zone-high]")?.value);
    zone.root_key = Number(zoneField("[data-zone-root]")?.value);
    zone.color = normalizeZoneColor(zoneField("[data-zone-color]")?.value);
    if (!zoneField("[data-zone-envelope-override]")?.checked) zone.envelope_override = null;
    const index = state.zones.findIndex((candidate) => candidate.id === zone.id);
    if (index >= 0) state.zones[index] = zone;
    else state.zones.push(zone);
    return state;
  };

  const prepareBuilderPreview = async () => {
    const state = auditionBuilderState();
    const validation = validateBuilderState(state, builderFiles);
    if (validation.error) throw Object.assign(new Error(validation.error), { waitingForDraft: true });
    const encoder = new TextEncoder();
    const samplePaths = new Map();
    const filePaths = new Map();
    const entries = [];
    for (const [layer, file] of validation.files) {
      let path = filePaths.get(file);
      if (!path) {
        const extension = file.name.match(/\.[^.]+$/)?.[0].toLowerCase() || ".wav";
        const stem = slugFile(file.name.replace(/\.[^.]+$/, ""));
        path = `samples/${String(filePaths.size + 1).padStart(3, "0")}-${stem}${extension}`;
        filePaths.set(file, path);
        entries.push({ name: path, data: new Uint8Array(await file.arrayBuffer()) });
      }
      samplePaths.set(layer.id, path);
    }
    const instrumentId = "builder-audition";
    const instrument = {
      ...(state._rf_extra || {}),
      schema_version: 1,
      id: instrumentId,
      name: validation.name,
      envelope: envelopeForExport(state.envelope),
      zones: state.zones.map((zone) => ({
        ...(zone._rf_extra || {}),
        name: zone.name,
        color: normalizeZoneColor(zone.color),
        key_low: zone.key_low,
        key_high: zone.key_high,
        root_key: zone.root_key,
        envelope_override: zone.envelope_override ? envelopeForExport(zone.envelope_override) : null,
        layers: zone.layers.map((layer) => ({
          ...(layer._rf_extra || {}),
          sample: samplePaths.get(layer.id),
          velocity_low: layer.velocity_low,
          velocity_high: layer.velocity_high,
          gain_db: layer.volume_percent <= 0 ? -60 : 20 * Math.log10(layer.volume_percent / 100),
          pan: layer.pan,
          tune_semitones: layer.tune_semitones,
        })),
      })),
    };
    entries.unshift(
      {
        name: "bank.json",
        data: encoder.encode(JSON.stringify({
          schema_version: 1,
          id: instrumentId,
          name: `${validation.name} · Builder audition`,
          instrument_count: 1,
          created_by: "RF-Soundfonts Instrument Builder",
        })),
      },
      {
        name: `instruments/${instrumentId}.rfinstrument`,
        data: encoder.encode(JSON.stringify(instrument)),
      },
    );
    const totalBytes = entries.reduce((total, entry) => total + entry.data.length, 0);
    if (entries.length > MAX_BANK_ENTRIES || totalBytes > MAX_BANK_BYTES) {
      throw new Error("The audition bank exceeds the 128 MB runtime limit.");
    }
    return { blob: makeZip(entries), name: validation.name };
  };

  const syncBuilderPreview = async () => {
    if (builderEditMode !== EDIT_MODE_ON || builderPreviewBusy) return;
    builderPreviewBusy = true;
    const revision = builderPreviewRevision;
    setBuilderAudition("syncing", "Syncing audition", "Preparing the current zones and samples…");
    try {
      const prepared = await prepareBuilderPreview();
      if (revision !== builderPreviewRevision) return;
      const bytes = await prepared.blob.arrayBuffer();
      await hostRequest("plugin.preview_resource", {
        edit_mode: builderEditMode,
        target_resource_id: "user-soundfont",
        file_name: "rf-builder-audition.rfbank",
        bytes,
      }, [bytes]);
      if (revision === builderPreviewRevision) {
        setBuilderAudition("live", "Audition live", `${prepared.name} now receives RackForge MIDI and feeds its audio output.`);
      }
    } catch (error) {
      if (revision !== builderPreviewRevision) return;
      if (error?.waitingForDraft) {
        setBuilderAudition("waiting", "Audition waiting", error.message);
      } else {
        setBuilderAudition("error", "Audition unavailable", error instanceof Error ? error.message : "RackForge could not load the draft.");
      }
    } finally {
      builderPreviewBusy = false;
      if (revision !== builderPreviewRevision) scheduleBuilderPreview(80);
    }
  };

  function scheduleBuilderPreview(delay = 450) {
    if (builderEditMode !== EDIT_MODE_ON) return;
    builderPreviewRevision += 1;
    window.clearTimeout(builderPreviewTimer);
    builderPreviewTimer = window.setTimeout(syncBuilderPreview, delay);
  }

  function beginBuilderAudition() {
    builderEditMode = EDIT_MODE_ON;
    if (builderAudition) builderAudition.dataset.editMode = String(builderEditMode);
    scheduleBuilderPreview(40);
  }

  const prepareBankEntries = async () => {
    syncOpenedInstrument();
    const descriptors = openedBank?.instruments || [{
      path: `instruments/${slugFile(builderState.name)}.rfinstrument`,
      foldedPath: `instruments/${slugFile(builderState.name)}.rfinstrument`.toLowerCase(),
      state: structuredClone(builderState),
      files: new Map(builderFiles),
    }];
    const encoder = new TextEncoder();
    const replacements = new Map();
    const addedEntries = new Map();
    const usedPaths = new Set(openedBank?.entries.map((entry) => entry.foldedName) || []);
    const assignedFilePaths = new Map();
    const instrumentNames = [];
    for (const descriptor of descriptors) {
      const validation = validateBuilderState(descriptor.state, descriptor.files);
      if (validation.error) throw new Error(`${descriptor.state.name}: ${validation.error}`);
      const state = descriptor.state;
      const instrumentSlug = slugFile(state._rf_id || validation.name);
      instrumentNames.push(validation.name);
      const samplePaths = new Map();
      for (const [layer, file] of validation.files) {
        let path = layer.sample_path ? normalizeZipPath(layer.sample_path) : "";
        if (!path) path = assignedFilePaths.get(file) || "";
        if (!path) {
          const stem = file.name.replace(/\.[^.]+$/, "").replace(/[^a-z0-9._-]+/gi, "-") || "sample";
          const extension = file.name.match(/\.[^.]+$/)?.[0].toLowerCase() || ".wav";
          let basename = `${stem}${extension}`;
          let suffix = 2;
          while (usedPaths.has(`samples/${basename}`.toLowerCase())) basename = `${stem}-${suffix++}${extension}`;
          path = `samples/${basename}`;
          usedPaths.add(path.toLowerCase());
        }
        layer.sample_path = path;
        layer.sample_name = path.split("/").at(-1);
        assignedFilePaths.set(file, path);
        samplePaths.set(layer.id, path);
        const foldedPath = path.toLocaleLowerCase("en-US");
        if (!openedBank?.entryByName.has(foldedPath) && !addedEntries.has(foldedPath)) {
          addedEntries.set(foldedPath, { name: path, data: new Uint8Array(await file.arrayBuffer()) });
        }
      }
      const instrument = {
        ...(state._rf_extra || {}),
        schema_version: 1,
        id: instrumentSlug,
        name: validation.name,
        envelope: envelopeForExport(state.envelope),
        zones: state.zones.map((zone) => ({
          ...(zone._rf_extra || {}),
          name: zone.name,
          color: normalizeZoneColor(zone.color),
          key_low: zone.key_low,
          key_high: zone.key_high,
          root_key: zone.root_key,
          envelope_override: zone.envelope_override ? envelopeForExport(zone.envelope_override) : null,
          layers: zone.layers.map((layer) => ({
            ...(layer._rf_extra || {}),
            sample: samplePaths.get(layer.id),
            velocity_low: layer.velocity_low,
            velocity_high: layer.velocity_high,
            gain_db: layer.volume_percent <= 0 ? -60 : 20 * Math.log10(layer.volume_percent / 100),
            pan: layer.pan,
            tune_semitones: layer.tune_semitones,
          })),
        })),
      };
      replacements.set(descriptor.foldedPath, {
        name: descriptor.path,
        data: encoder.encode(JSON.stringify(instrument, null, 2)),
      });
    }
    const primarySlug = slugFile(openedBank?.manifest?.id || instrumentNames[0]);
    const manifest = openedBank ? {
      ...openedBank.manifest,
      schema_version: 1,
      id: String(openedBank.manifest.id || primarySlug),
      name: String(openedBank.manifest.name || openedBank.fileName.replace(/\.rfbank$/i, "")),
      instrument_count: descriptors.length + openedBank.legacyCount,
    } : {
      schema_version: 1,
      id: primarySlug,
      name: instrumentNames[0],
      instrument_count: 1,
      created_by: "RF-Soundfonts Instrument Builder",
    };
    const manifestPath = openedBank?.manifestPath || "bank.json";
    replacements.set(manifestPath.toLocaleLowerCase("en-US"), {
      name: manifestPath,
      data: encoder.encode(JSON.stringify(manifest, null, 2)),
    });

    const entries = [];
    const consumed = new Set();
    if (openedBank) {
      for (const entry of openedBank.entries) {
        const replacement = replacements.get(entry.foldedName);
        entries.push(replacement || { name: entry.name, data: await extractZipEntry(openedBank, entry) });
        if (replacement) consumed.add(entry.foldedName);
      }
    }
    for (const [path, replacement] of replacements) {
      if (!consumed.has(path)) entries.push(replacement);
    }
    entries.push(...addedEntries.values());
    const totalBytes = entries.reduce((total, entry) => total + entry.data.length, 0);
    if (entries.length > MAX_BANK_ENTRIES || totalBytes > MAX_BANK_BYTES) {
      throw new Error("The edited RF Bank exceeds the 8,192-entry or 128 MB runtime limit.");
    }
    return {
      blob: makeZip(entries),
      suggestedName: openedBank?.fileName || `${primarySlug}.rfbank`,
      label: openedBank?.manifest?.name || instrumentNames[0],
    };
  };

  const saveBankBlob = async (prepared, mode, forceDownload = false) => {
    let handle = mode === "same" ? openedBank?.handle : null;
    if (!handle && !forceDownload && typeof window.showSaveFilePicker === "function") {
      handle = await window.showSaveFilePicker({
        suggestedName: prepared.suggestedName,
        types: [{
          description: "RF Instrument Bank",
          accept: { "application/vnd.rackforge.bank+zip": [".rfbank"] },
        }],
      });
    }
    if (handle?.createWritable) {
      const writable = await handle.createWritable();
      await writable.write(prepared.blob);
      await writable.close();
      if (openedBank) {
        openedBank.handle = handle;
        openedBank.fileName = handle.name || prepared.suggestedName;
        renderOpenedBankContext();
      }
      return;
    }
    const url = URL.createObjectURL(prepared.blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = prepared.suggestedName;
    anchor.click();
    window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
  };

  const runBankSave = async (mode, forceDownload = false) => {
    builderExport.disabled = true;
    if (builderSave) builderSave.disabled = true;
    setBuilderMessage(openedBank ? "Repacking RF Bank without dropping preserved content…" : "Packing instrument and samples…");
    try {
      const prepared = await prepareBankEntries();
      await saveBankBlob(prepared, mode, forceDownload);
      setBuilderMessage(`${prepared.label} saved successfully.`, "success");
    } catch (error) {
      if (error?.name !== "AbortError") {
        setBuilderMessage(error instanceof Error ? error.message : "The RF Bank could not be saved.", "error");
      }
    } finally {
      builderExport.disabled = false;
      if (builderSave) builderSave.disabled = false;
    }
  };

  if (builderExport) builderExport.title = "Save a copy · Hold Shift to use the portable download fallback";
  builderExport?.addEventListener("click", (event) => runBankSave("as", event.shiftKey));
  builderSave?.addEventListener("click", () => runBankSave("same"));

  if (builderViewport) {
    builderName.value = builderState.name;
    renderEnvelope(globalEnvelopeHost, builderState.envelope, () => {
      beginBuilderAudition();
      persistBuilder();
    });
    renderBuilderKeys();
    renderBuilderZones();
    renderOpenedBankContext();
    updateBuilderZoom(builderRowHeight);
    document.querySelector('[data-tab="builder"]')?.addEventListener("click", () => {
      if (builderDidInitialScroll) return;
      builderDidInitialScroll = true;
      window.requestAnimationFrame(() => {
        builderViewport.scrollTop = (127 - 72) * builderRowHeight;
      });
    });
  }

  window.addEventListener("message", (event) => {
    if (
      event.origin !== hostOrigin ||
      event.source !== window.parent ||
      event.data?.protocol !== PROTOCOL
    ) return;
    if (event.data.kind === "response") {
      const request = pending.get(event.data.request_id);
      if (!request) return;
      pending.delete(event.data.request_id);
      window.clearTimeout(request.timer);
      if (event.data.ok) request.resolve(event.data.result);
      else request.reject(new Error(event.data.error || "RackForge rejected the request."));
      return;
    }
    if (event.data.kind === "context" && event.data.instance) {
      contextRevision += 1;
      renderSounds(event.data.instance);
      refreshVolume();
      finishLibraryActivation(event.data.instance);
    }
  });

  window.parent.postMessage({
    protocol: PROTOCOL,
    kind: "ready",
  }, hostOrigin);

  if (selection) {
    Promise.all([refreshAddedLibraries(), refreshInstalled()]).then(([libraries, installed]) => {
      if (!installed) {
        rememberActiveLibrary(null);
      } else if (!libraries.some((grant) => grant.grant_id === activeGrantId)) {
        const fallback = libraries.at(-1);
        if (fallback) {
          rememberActiveLibrary(fallback);
          installedName = fallback.display_name || null;
        }
      } else {
        installedName = libraries.find((grant) => grant.grant_id === activeGrantId)?.display_name || null;
      }
      renderAddedLibraries();
      showInstalled(installed);
    });
  }

  if (soundList) {
    refreshAddedLibraries();
    window.addEventListener("storage", (event) => {
      if (event.key === "rf-soundfonts.active-library") {
        activeGrantId = event.newValue;
      } else if (event.key === LIBRARY_CATALOGS_KEY) {
        try {
          libraryCatalogs = JSON.parse(event.newValue || "{}");
        } catch (_) {
          libraryCatalogs = {};
        }
      } else {
        return;
      }
      refreshAddedLibraries();
    });
  }
})();
