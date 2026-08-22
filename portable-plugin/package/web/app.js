(() => {
  "use strict";

  const PROTOCOL = "rackforge.plugin.web@1";
  const hostOrigin = window.location.origin;
  const pending = new Map();
  let requestSerial = 0;

  const requestTimeout = (method) => {
    if (method === "plugin.select_resource") return 5 * 60_000;
    if (method === "plugin.install_resource" || method === "plugin.clear_resource") {
      return 2 * 60_000;
    }
    return 20_000;
  };

  const hostRequest = (method, params = {}) =>
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
      }, hostOrigin);
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
    return name.replace(/\.(?:nki|sf2|rfbank)$/i, "");
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
        bundle: /\.nki$/i.test(grant.display_name || "") ? "nki_dependencies" : null,
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
        bundle: /\.nki$/i.test(name) ? "nki_dependencies" : null,
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
        extensions: ["nki", "sf2", "rfbank"],
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
  const builderNewZone = document.querySelector("[data-builder-new-zone]");
  const builderExport = document.querySelector("[data-builder-export]");
  const builderSamples = document.querySelector("[data-builder-samples]");
  const globalEnvelopeHost = document.querySelector("[data-global-envelope]");
  const zoneDialog = document.querySelector("[data-zone-dialog]");
  const zoneEnvelopeHost = document.querySelector("[data-zone-envelope]");
  const layerList = document.querySelector("[data-layer-list]");
  const BUILDER_DRAFT_KEY = "rf-soundfonts.instrument-builder.v1";
  const SAMPLE_LIMIT_BYTES = 160 * 1_048_576;
  let builderRowHeight = 28;
  let selectedZoneId = null;
  let editingZone = null;
  let sampleTargetLayerId = null;
  let builderDidInitialScroll = false;
  const builderFiles = new Map();

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
  };

  const setBuilderMessage = (message, kind = "") => {
    if (!builderMessage) return;
    builderMessage.textContent = message;
    builderMessage.toggleAttribute("data-error", kind === "error");
    builderMessage.toggleAttribute("data-success", kind === "success");
  };

  const envelopeDefinitions = [
    { key: "attack", label: "Attack", min: 0, max: 5, step: 0.001, unit: "s" },
    { key: "decay", label: "Decay", min: 0, max: 5, step: 0.001, unit: "s" },
    { key: "sustain", label: "Sustain", min: 0, max: 1, step: 0.01, unit: "%" },
    { key: "release", label: "Release", min: 0, max: 10, step: 0.001, unit: "s" },
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
    gain_db: 0,
    pan: 0,
    tune_semitones: 0,
  });

  const newZone = (note = 60) => ({
    id: builderId("zone"),
    name: `Zone ${noteName(note)}`,
    key_low: note,
    key_high: note,
    root_key: note,
    envelope_override: null,
    layers: [newLayer()],
  });

  const renderBuilderKeys = () => {
    if (!builderKeys) return;
    const fragment = document.createDocumentFragment();
    const black = new Set([1, 3, 6, 8, 10]);
    for (let note = 127; note >= 0; note -= 1) {
      const key = document.createElement("div");
      key.className = `builder-key${black.has(note % 12) ? " black" : ""}${note === 60 ? " middle-c" : ""}`;
      key.style.top = `calc(var(--builder-row) * ${127 - note})`;
      key.textContent = `${noteName(note)}  ${note}`;
      key.dataset.note = String(note);
      key.addEventListener("dblclick", () => openZoneEditor(newZone(note)));
      fragment.append(key);
    }
    builderKeys.replaceChildren(fragment);
  };

  const renderBuilderZones = () => {
    if (!builderLane) return;
    const fragment = document.createDocumentFragment();
    for (const [index, zone] of builderState.zones.entries()) {
      const pill = document.createElement("button");
      pill.type = "button";
      pill.className = `zone-pill${zone.id === selectedZoneId ? " selected" : ""}`;
      pill.style.top = `calc(var(--builder-row) * ${127 - zone.key_high} + 2px)`;
      pill.style.height = `calc(var(--builder-row) * ${zone.key_high - zone.key_low + 1} - 4px)`;
      pill.style.left = `${12 + (index % 4) * 7}px`;
      pill.style.right = `${12 + ((index + 1) % 4) * 7}px`;
      pill.dataset.zoneId = zone.id;
      const title = document.createElement("strong");
      title.textContent = zone.name || `Zone ${index + 1}`;
      const detail = document.createElement("span");
      detail.textContent = `${noteName(zone.key_low)}–${noteName(zone.key_high)} · ${zone.layers.length} layer${zone.layers.length === 1 ? "" : "s"}${zone.envelope_override ? " · ADSR override" : ""}`;
      pill.append(title, detail);
      pill.addEventListener("click", () => {
        selectedZoneId = zone.id;
        renderBuilderZones();
      });
      pill.addEventListener("dblclick", () => openZoneEditor(zone));
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
    for (const layer of editingZone.layers) {
      const row = document.createElement("div");
      row.className = "layer-row";
      const sampleField = document.createElement("label");
      sampleField.className = "layer-field sample-field";
      const sampleCaption = document.createElement("span");
      sampleCaption.textContent = "Sample";
      const sampleButton = document.createElement("button");
      sampleButton.type = "button";
      sampleButton.className = "sample-pick";
      sampleButton.textContent = layer.sample_name || "Choose WAV / FLAC";
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
        ["gain_db", "Gain dB", -60, 24, 0.1],
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
        input.addEventListener("input", () => { layer[key] = Number(input.value); });
        field.append(caption, input);
        row.append(field);
      }
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "layer-remove";
      remove.textContent = "×";
      remove.title = "Remove layer";
      remove.disabled = editingZone.layers.length === 1;
      remove.addEventListener("click", () => {
        editingZone.layers = editingZone.layers.filter((candidate) => candidate.id !== layer.id);
        builderFiles.delete(layer.id);
        renderLayerList();
      });
      row.append(remove);
      fragment.append(row);
    }
    layerList.replaceChildren(fragment);
  };

  const zoneField = (selector) => zoneDialog?.querySelector(selector);

  const openZoneEditor = (zone) => {
    if (!zoneDialog) return;
    editingZone = structuredClone(zone);
    selectedZoneId = zone.id;
    zoneField("[data-zone-name]").value = editingZone.name;
    zoneField("[data-zone-low]").value = editingZone.key_low;
    zoneField("[data-zone-high]").value = editingZone.key_high;
    zoneField("[data-zone-root]").value = editingZone.root_key;
    const override = zoneField("[data-zone-envelope-override]");
    override.checked = Boolean(editingZone.envelope_override);
    if (!editingZone.envelope_override) editingZone.envelope_override = structuredClone(builderState.envelope);
    zoneEnvelopeHost?.setAttribute("aria-disabled", String(!override.checked));
    renderEnvelope(zoneEnvelopeHost, editingZone.envelope_override);
    renderLayerList();
    zoneDialog.showModal();
    zoneField("[data-zone-name]").focus();
  };

  const closeZoneEditor = () => {
    editingZone = null;
    if (zoneDialog?.open) zoneDialog.close();
  };

  zoneField("[data-zone-envelope-override]")?.addEventListener("change", (event) => {
    zoneEnvelopeHost?.setAttribute("aria-disabled", String(!event.target.checked));
  });
  zoneField("[data-zone-close]")?.addEventListener("click", closeZoneEditor);
  zoneField("[data-zone-cancel]")?.addEventListener("click", closeZoneEditor);
  zoneDialog?.addEventListener("cancel", () => { editingZone = null; });
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
  });

  builderSamples?.addEventListener("change", () => {
    if (!editingZone || !sampleTargetLayerId) return;
    const files = [...builderSamples.files];
    if (!files.length) return;
    const accepted = files.filter((file) => /\.(wav|wave|flac)$/i.test(file.name));
    if (accepted.length !== files.length) {
      setBuilderMessage("Only WAV, WAVE and FLAC samples are supported.", "error");
    }
    const targetIndex = editingZone.layers.findIndex((layer) => layer.id === sampleTargetLayerId);
    accepted.forEach((file, index) => {
      let layer = editingZone.layers[targetIndex + index];
      if (!layer) {
        layer = newLayer();
        editingZone.layers.push(layer);
      }
      layer.sample_name = file.name;
      builderFiles.set(layer.id, file);
    });
    builderSamples.value = "";
    sampleTargetLayerId = null;
    renderLayerList();
  });

  const readZoneFields = () => {
    if (!editingZone) return null;
    editingZone.name = zoneField("[data-zone-name]").value.trim() || "Untitled zone";
    editingZone.key_low = Number(zoneField("[data-zone-low]").value);
    editingZone.key_high = Number(zoneField("[data-zone-high]").value);
    editingZone.root_key = Number(zoneField("[data-zone-root]").value);
    if (!zoneField("[data-zone-envelope-override]").checked) editingZone.envelope_override = null;
    return editingZone;
  };

  const validateZone = (zone) => {
    if (!Number.isInteger(zone.key_low) || !Number.isInteger(zone.key_high)
        || !Number.isInteger(zone.root_key) || zone.key_low < 0 || zone.key_high > 127
        || zone.key_low > zone.key_high || zone.root_key < zone.key_low || zone.root_key > zone.key_high) {
      return "The zone key range is invalid, or the root key is outside it.";
    }
    if (!zone.layers.length) return "The zone needs at least one velocity layer.";
    for (const layer of zone.layers) {
      if (!Number.isInteger(layer.velocity_low) || !Number.isInteger(layer.velocity_high)
          || layer.velocity_low < 1 || layer.velocity_high > 127
          || layer.velocity_low > layer.velocity_high) {
        return `Velocity range is invalid in ${layer.sample_name || "an unassigned layer"}.`;
      }
      if (!Number.isFinite(layer.gain_db) || layer.gain_db < -60 || layer.gain_db > 24
          || !Number.isFinite(layer.pan) || layer.pan < -1 || layer.pan > 1
          || !Number.isFinite(layer.tune_semitones) || layer.tune_semitones < -48 || layer.tune_semitones > 48) {
        return `Gain, pan or tuning is invalid in ${layer.sample_name || "an unassigned layer"}.`;
      }
    }
    return null;
  };

  zoneField("[data-zone-save]")?.addEventListener("click", () => {
    const zone = readZoneFields();
    const error = zone && validateZone(zone);
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
      for (const layer of editingZone.layers) builderFiles.delete(layer.id);
      builderState.zones = builderState.zones.filter((zone) => zone.id !== editingZone.id);
      selectedZoneId = null;
      persistBuilder();
      renderBuilderZones();
      setBuilderMessage("Zone deleted.");
    }
    closeZoneEditor();
  });

  builderNewZone?.addEventListener("click", () => openZoneEditor(newZone(60)));
  builderName?.addEventListener("input", () => {
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

  const validateBuilder = () => {
    const name = builderState.name.trim();
    if (!name) return { error: "Give the instrument a name before exporting." };
    if (!builderState.zones.length) return { error: "Add at least one key zone before exporting." };
    const files = [];
    let totalBytes = 0;
    for (const zone of builderState.zones) {
      const zoneError = validateZone(zone);
      if (zoneError) return { error: `${zone.name}: ${zoneError}` };
      for (const layer of zone.layers) {
        const file = builderFiles.get(layer.id);
        if (!file) return { error: `${zone.name}: choose the sample for ${layer.sample_name || "every velocity layer"}.` };
        totalBytes += file.size;
        files.push([layer, file]);
      }
    }
    if (totalBytes > SAMPLE_LIMIT_BYTES) {
      return { error: "This instrument exceeds the 160 MB decoded-sample limit." };
    }
    return { name, files };
  };

  builderExport?.addEventListener("click", async () => {
    const validation = validateBuilder();
    if (validation.error) {
      setBuilderMessage(validation.error, "error");
      return;
    }
    builderExport.disabled = true;
    setBuilderMessage("Packing instrument and samples…");
    try {
      const instrumentSlug = slugFile(validation.name);
      const usedNames = new Set();
      const sampleEntries = [];
      const sampleNames = new Map();
      for (const [layer, file] of validation.files) {
        const stem = file.name.replace(/\.[^.]+$/, "").replace(/[^a-z0-9._-]+/gi, "-") || "sample";
        const extension = file.name.match(/\.[^.]+$/)?.[0].toLowerCase() || ".wav";
        let name = `${stem}${extension}`;
        let suffix = 2;
        while (usedNames.has(name.toLowerCase())) name = `${stem}-${suffix++}${extension}`;
        usedNames.add(name.toLowerCase());
        sampleNames.set(layer.id, name);
        sampleEntries.push({ name: `samples/${name}`, data: new Uint8Array(await file.arrayBuffer()) });
      }
      const instrument = {
        schema_version: 1,
        id: instrumentSlug,
        name: validation.name,
        envelope: {
          attack_seconds: builderState.envelope.attack,
          decay_seconds: builderState.envelope.decay,
          sustain_level: builderState.envelope.sustain,
          release_seconds: builderState.envelope.release,
        },
        zones: builderState.zones.map((zone) => ({
          name: zone.name,
          key_low: zone.key_low,
          key_high: zone.key_high,
          root_key: zone.root_key,
          envelope_override: zone.envelope_override ? {
            attack_seconds: zone.envelope_override.attack,
            decay_seconds: zone.envelope_override.decay,
            sustain_level: zone.envelope_override.sustain,
            release_seconds: zone.envelope_override.release,
          } : null,
          layers: zone.layers.map((layer) => ({
            sample: sampleNames.get(layer.id),
            velocity_low: layer.velocity_low,
            velocity_high: layer.velocity_high,
            gain_db: layer.gain_db,
            pan: layer.pan,
            tune_semitones: layer.tune_semitones,
          })),
        })),
      };
      const encoder = new TextEncoder();
      const entries = [
        { name: "bank.json", data: encoder.encode(JSON.stringify({
          schema_version: 1,
          id: instrumentSlug,
          name: validation.name,
          instrument_count: 1,
          created_by: "RF-Soundfonts Instrument Builder",
        }, null, 2)) },
        { name: `instruments/${instrumentSlug}.rfinstrument`, data: encoder.encode(JSON.stringify(instrument, null, 2)) },
        ...sampleEntries,
      ];
      const blob = makeZip(entries);
      if (typeof window.showSaveFilePicker === "function") {
        const handle = await window.showSaveFilePicker({
          suggestedName: `${instrumentSlug}.rfbank`,
          types: [{
            description: "RF Instrument Bank",
            accept: { "application/vnd.rackforge.bank+zip": [".rfbank"] },
          }],
        });
        const writable = await handle.createWritable();
        await writable.write(blob);
        await writable.close();
      } else {
        const url = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = `${instrumentSlug}.rfbank`;
        anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
      }
      setBuilderMessage(`${validation.name}.rfbank exported. Add it from the Library tab to play it.`, "success");
    } catch (error) {
      setBuilderMessage(error instanceof Error ? error.message : "The RF Bank could not be exported.", "error");
    } finally {
      builderExport.disabled = false;
    }
  });

  if (builderViewport) {
    builderName.value = builderState.name;
    renderEnvelope(globalEnvelopeHost, builderState.envelope, persistBuilder);
    renderBuilderKeys();
    renderBuilderZones();
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
