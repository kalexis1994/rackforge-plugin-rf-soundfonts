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

  const keyboard = document.querySelector(".keyboard");
  if (keyboard) {
    const blackNotes = new Set([1, 3, 6, 8, 10]);
    const fragment = document.createDocumentFragment();
    for (let note = 0; note < 36; note += 1) {
      const key = document.createElement("i");
      key.className = blackNotes.has(note % 12) ? "black" : "white";
      fragment.append(key);
    }
    keyboard.replaceChildren(fragment);
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
  const instrumentRack = document.querySelector(".instrument-rack");
  const volume = document.querySelector("[data-volume]");
  const volumeValue = document.querySelector("[data-volume-value]");
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

  // PLAY is a safe renderer, not the identity of every bank. A bank profile
  // can select one of the supported compositions and supply its own artwork,
  // palette and copy, but it cannot inject markup, script or arbitrary CSS.
  const BANK_LAYOUTS = new Set(["studio", "cinematic", "compact", "minimal"]);
  const BANK_MODULES = new Set(["artwork", "instrument", "controls", "keyboard"]);
  const DEFAULT_BANK_PRESENTATION = {
    id: "generic-soundfont",
    layout: "studio",
    theme: {
      ground: "#091413",
      surface: "#0d1f1c",
      accent: "#b0e4cc",
      structure: "#408a71",
    },
    copy: {
      edition: "SOUNDFONT BANK",
      kicker: "LOADED INSTRUMENT",
      control_kicker: "CHANNEL",
      control_title: "Output",
      engine: "WASM · 96 VOICES",
      footer: "RACKFORGE INSTRUMENT",
      credit: "SF2 · PORTABLE",
    },
    modules: ["instrument", "controls", "keyboard"],
    keyboard: true,
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
      keyboard: value.keyboard !== false,
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

  const presentationFor = (instance, sound) => {
    const bank = (Array.isArray(instance?.banks) ? instance.banks : [])
      .find((candidate) => candidate.id === sound?.bank);
    return bankPresentations.profiles.find((profile) => {
      const match = profile.match;
      return (
        (match.bank_ids.length && match.bank_ids.includes(bank?.id ?? sound?.bank)) ||
        (match.bank_name_contains.length && includesAny(bank?.name, match.bank_name_contains)) ||
        (match.sound_name_contains.length && includesAny(sound?.name, match.sound_name_contains))
      );
    }) ?? bankPresentations.fallback;
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
      module.hidden = !enabled.has(name) || (name === "keyboard" && !presentation.keyboard);
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
    if (bankControlKicker) bankControlKicker.textContent = presentation.copy.control_kicker;
    if (bankControlTitle) bankControlTitle.textContent = presentation.copy.control_title;
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

  const bankNameFor = (instance, sound) => {
    const banks = Array.isArray(instance?.banks) ? instance.banks : [];
    return banks.find((bank) => bank.id === sound?.bank)?.name
      ?? sound?.bank
      ?? (banks.length === 1 ? banks[0].name : null)
      ?? "Factory bank";
  };

  const selectSound = async (sound) => {
    if (!currentInstance || soundSelectionBusy || sound.id === currentInstance.selected_sound_id) {
      return;
    }
    soundSelectionBusy = true;
    soundBrowser?.setAttribute("aria-busy", "true");
    instrumentRack?.setAttribute("aria-busy", "true");
    for (const button of soundList?.querySelectorAll("button") ?? []) button.disabled = true;
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
      soundBrowser?.removeAttribute("aria-busy");
      instrumentRack?.removeAttribute("aria-busy");
      for (const button of soundList?.querySelectorAll("button") ?? []) button.disabled = false;
    }
  };

  const renderSounds = (instance) => {
    if (!soundList || !soundBrowser) return;
    currentInstance = instance;
    const sounds = Array.isArray(instance?.sounds) ? instance.sounds : [];
    const banks = Array.isArray(instance?.banks) ? instance.banks : [];
    const bankNames = new Map(banks.map((bank) => [bank.id, bank.name]));
    const selected = sounds.find((sound) => sound.id === instance?.selected_sound_id)
      ?? sounds[0]
      ?? null;

    applyBankPresentation(instance, selected);

    if (soundTitle) soundTitle.textContent = selected?.name ?? "No instrument loaded";
    if (soundSubtitle) {
      soundSubtitle.textContent = selected?.detail
        ?? (sounds.length
          ? `${sounds.length} sounds available in the loaded bank.`
          : "Open CONFIG to install or restore a SoundFont bank.");
    }
    if (activeBank) activeBank.textContent = bankNameFor(instance, selected);
    setPlayStatus(sounds.length ? "READY" : "NO BANK");

    const query = soundFilter.trim().toLocaleLowerCase();
    const visible = query
      ? sounds.filter((sound) =>
        [sound.name, sound.detail, bankNames.get(sound.bank)]
          .some((value) => String(value ?? "").toLocaleLowerCase().includes(query)))
      : sounds;
    if (soundCount) {
      soundCount.textContent = query
        ? `${visible.length} / ${sounds.length}`
        : `${sounds.length} SOUND${sounds.length === 1 ? "" : "S"}`;
    }

    const fragment = document.createDocumentFragment();
    if (!visible.length) {
      const empty = document.createElement("span");
      empty.className = "sound-bank";
      empty.textContent = sounds.length
        ? "No instruments match this search."
        : "No bank loaded.";
      fragment.append(empty);
    } else {
      let currentBank = Symbol("initial-bank");
      const showBankHeadings =
        banks.length > 1 || new Set(visible.map((sound) => sound.bank)).size > 1;
      for (const sound of visible) {
        if (showBankHeadings && sound.bank !== currentBank) {
          currentBank = sound.bank;
          const heading = document.createElement("span");
          heading.className = "sound-bank";
          heading.textContent = bankNames.get(sound.bank) ?? sound.bank ?? "Bank";
          fragment.append(heading);
        }
        const button = document.createElement("button");
        button.type = "button";
        button.className = "sound-entry";
        button.setAttribute("role", "option");
        button.setAttribute("aria-selected", String(sound.id === selected?.id));
        button.textContent = sound.name;
        if (sound.id === selected?.id) button.classList.add("selected");
        button.addEventListener("click", () => selectSound(sound));
        fragment.append(button);
      }
    }
    soundList.replaceChildren(fragment);
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
      })
      .catch(() => {
        volume.disabled = true;
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

  // CONFIG: authorized folder browser and installed-bank lifecycle.
  const selectLibrary = document.querySelector("[data-select-library]");
  const selection = document.querySelector("[data-library-selection]");
  const libraryFiles = document.querySelector("[data-library-files]");
  const libraryEntries = document.querySelector("[data-library-entries]");
  const libraryPath = document.querySelector("[data-library-path]");
  const configStatus = document.querySelector("[data-config-status]");
  const factoryCard = document.querySelector("[data-factory-card]");
  const noBankCard = document.querySelector("[data-no-bank]");
  const clearBank = document.querySelector("[data-clear-bank]");
  const installedBankName = document.querySelector("[data-installed-bank-name]");
  const installedBankDetail = document.querySelector("[data-installed-bank-detail]");
  let browseGeneration = 0;
  let installedName = null;

  const setConfigMessage = (message) => {
    if (selection) selection.textContent = message;
  };

  const setLibraryBusy = (busy) => {
    libraryFiles?.toggleAttribute("aria-busy", busy);
    selectLibrary?.toggleAttribute("disabled", busy);
    for (const button of libraryEntries?.querySelectorAll("button") ?? []) {
      button.disabled = busy;
    }
  };

  const showInstalled = (installed) => {
    if (factoryCard) factoryCard.hidden = !installed;
    if (noBankCard) noBankCard.hidden = installed;
    if (configStatus) configStatus.textContent = installed ? "USER BANK ACTIVE" : "FACTORY BANK ACTIVE";
    if (installedBankName) installedBankName.textContent = installedName || "Installed user bank";
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

  const installBank = async (grant, entry) => {
    setLibraryBusy(true);
    setConfigMessage(`Installing ${entry.name}…`);
    if (configStatus) configStatus.textContent = "INSTALLING BANK";
    try {
      await hostRequest("plugin.install_resource", {
        target_resource_id: "user-soundfont",
        grant_id: grant.grant_id,
        entry_id: entry.id,
      });
      installedName = entry.name;
      for (const button of libraryEntries?.querySelectorAll("button") ?? []) {
        button.classList.toggle("selected", button.dataset.entryId === entry.id);
      }
      setConfigMessage(`Installed and playing: ${entry.name}`);
      await refreshInstalled();
    } catch (error) {
      setConfigMessage(error instanceof Error ? error.message : "The bank could not be installed.");
      if (configStatus) configStatus.textContent = "INSTALL FAILED";
    } finally {
      setLibraryBusy(false);
    }
  };

  const showLibrary = async (grant, parentId = null, trail = []) => {
    if (!libraryFiles || !libraryEntries || !grant?.grant_id) return;
    const generation = browseGeneration += 1;
    libraryFiles.hidden = false;
    setLibraryBusy(true);
    libraryEntries.textContent = "Reading library…";
    if (libraryPath) {
      libraryPath.textContent = trail.length
        ? trail.map((item) => item.name).join(" / ")
        : "Library root";
    }
    try {
      const response = await hostRequest("plugin.resource_entries", {
        grant_id: grant.grant_id,
        parent_id: parentId,
      });
      if (generation !== browseGeneration) return;
      const entries = Array.isArray(response) ? response : [];
      const visible = entries
        .filter((entry) => entry?.kind === "directory" || /\.sf2$/i.test(entry?.name ?? ""))
        .sort((left, right) => {
          if (left.kind !== right.kind) return left.kind === "directory" ? -1 : 1;
          return String(left.name).localeCompare(String(right.name), undefined, {
            numeric: true,
            sensitivity: "base",
          });
        });
      const fragment = document.createDocumentFragment();

      if (trail.length) {
        const parentTrail = trail.slice(0, -1);
        const back = document.createElement("button");
        back.type = "button";
        back.className = "library-entry";
        back.textContent = `‹  ${parentTrail.at(-1)?.name ?? "Library root"}`;
        back.addEventListener("click", () =>
          showLibrary(grant, parentTrail.at(-1)?.id ?? null, parentTrail));
        fragment.append(back);
      }

      if (!visible.length) {
        const empty = document.createElement("span");
        empty.className = "empty";
        empty.textContent = "No compatible .sf2 banks were found in this folder.";
        fragment.append(empty);
      } else {
        for (const entry of visible) {
          const button = document.createElement("button");
          button.type = "button";
          button.className = "library-entry";
          button.textContent = `${entry.kind === "directory" ? "▸" : "♪"}  ${entry.name}`;
          if (entry.kind === "directory") {
            button.addEventListener("click", () =>
              showLibrary(grant, entry.id, [...trail, { id: entry.id, name: entry.name }]));
          } else {
            button.dataset.entryId = entry.id;
            button.addEventListener("click", () => installBank(grant, entry));
          }
          fragment.append(button);
        }
      }
      libraryEntries.replaceChildren(fragment);
    } catch (error) {
      if (generation === browseGeneration) {
        libraryEntries.textContent =
          error instanceof Error ? error.message : "The authorized folder could not be read.";
      }
    } finally {
      if (generation === browseGeneration) setLibraryBusy(false);
    }
  };

  selectLibrary?.addEventListener("click", async () => {
    selectLibrary.disabled = true;
    setConfigMessage("Waiting for RackForge…");
    try {
      const grant = await hostRequest("plugin.select_resource", {
        resource_id: "user-library",
      });
      if (!grant?.grant_id) throw new Error("RackForge did not return a valid folder grant.");
      setConfigMessage(grant.display_name
        ? `Authorized: ${grant.display_name}`
        : "Library folder authorized.");
      await showLibrary(grant);
    } catch (error) {
      setConfigMessage(error instanceof Error ? error.message : "Folder selection failed.");
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
        showInstalled(false);
        setConfigMessage("Factory YDP Grand Piano restored.");
        for (const entry of libraryEntries?.querySelectorAll("button") ?? []) {
          entry.classList.remove("selected");
        }
      } catch (error) {
        setConfigMessage(error instanceof Error ? error.message : "The bank could not be cleared.");
        if (configStatus) configStatus.textContent = "RESTORE FAILED";
      } finally {
        clearBank.disabled = false;
      }
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
      renderSounds(event.data.instance);
      refreshVolume();
    }
  });

  window.parent.postMessage({
    protocol: PROTOCOL,
    kind: "ready",
  }, hostOrigin);

  if (selection) {
    refreshInstalled();
    hostRequest("plugin.resource_bindings", {})
      .then((grants) => {
        const values = Array.isArray(grants) ? grants : [];
        const grant = values.find((candidate) => candidate.resource_id === "user-library");
        if (!grant) return null;
        setConfigMessage(`Authorized: ${grant.display_name || "SoundFont library"}`);
        return showLibrary(grant);
      })
      .catch(() => {
        // No persistent grant is the expected first-run state.
      });
  }
})();
