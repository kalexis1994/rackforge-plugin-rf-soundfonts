(() => {
  "use strict";
  const keyboard = document.querySelector(".keyboard");
  if (keyboard) {
    const blackNotes = new Set([1, 3, 6, 8, 10]);
    for (let note = 0; note < 36; note += 1) {
      const key = document.createElement("i");
      key.className = blackNotes.has(note % 12) ? "black" : "white";
      keyboard.append(key);
    }
  }

  const pending = new Map();
  let requestSerial = 0;
  const hostRequest = (method, params) =>
    new Promise((resolve, reject) => {
      const requestId = `rf-soundfonts-${Date.now()}-${requestSerial += 1}`;
      pending.set(requestId, { resolve, reject });
      window.parent.postMessage({
        protocol: "rackforge.plugin.web@1",
        kind: "request",
        request_id: requestId,
        method,
        params,
      }, window.location.origin);
    });

  // PLAY: sound browser and master volume ---------------------------------

  const soundTitle = document.querySelector("[data-sound-title]");
  const soundSubtitle = document.querySelector("[data-sound-subtitle]");
  const soundBrowser = document.querySelector("[data-sound-browser]");
  const soundList = document.querySelector("[data-sound-list]");
  const playStatus = document.querySelector("[data-play-status]");
  const volume = document.querySelector("[data-volume]");
  const volumeValue = document.querySelector("[data-volume-value]");

  const showVolume = (value) => {
    if (!volume || !volumeValue) return;
    volume.value = String(value);
    volumeValue.textContent = `${Math.round(value * 100)}%`;
  };

  if (volume) {
    let sendTimer = null;
    volume.addEventListener("input", () => {
      const value = Number(volume.value);
      if (volumeValue) volumeValue.textContent = `${Math.round(value * 100)}%`;
      // Collapse a drag into at most one request per frame-ish interval.
      if (sendTimer) return;
      sendTimer = setTimeout(() => {
        sendTimer = null;
        hostRequest("plugin.set_parameter", {
          parameter_index: 0,
          value: Number(volume.value),
        }).catch(() => {
          // The next context or parameter read restores the real value.
        });
      }, 60);
    });
  }

  const renderSounds = (instance) => {
    if (!soundList || !soundBrowser) return;
    const sounds = Array.isArray(instance?.sounds) ? instance.sounds : [];
    const bankNames = new Map(
      (Array.isArray(instance?.banks) ? instance.banks : [])
        .map((bank) => [bank.id, bank.name]),
    );
    const selected = sounds.find((sound) => sound.id === instance.selected_sound_id)
      ?? sounds[0];
    if (soundTitle && selected) soundTitle.textContent = selected.name;
    if (soundSubtitle) {
      soundSubtitle.textContent = selected?.detail
        ?? (sounds.length > 1
          ? `${sounds.length} sounds in the loaded bank.`
          : "The default RackForge acoustic piano.");
    }
    if (playStatus) playStatus.textContent = sounds.length ? "READY" : "NO BANK";
    soundBrowser.hidden = sounds.length < 2;
    soundList.replaceChildren();
    let currentBank;
    for (const sound of sounds) {
      if (sound.bank !== currentBank && bankNames.size > 1) {
        currentBank = sound.bank;
        const heading = document.createElement("span");
        heading.className = "sound-bank";
        heading.textContent = bankNames.get(sound.bank) ?? sound.bank ?? "Bank";
        soundList.append(heading);
      }
      const button = document.createElement("button");
      button.type = "button";
      button.className = "sound-entry";
      button.textContent = sound.name;
      if (sound.id === instance.selected_sound_id) button.classList.add("selected");
      button.addEventListener("click", async () => {
        for (const entry of soundList.children) entry.disabled = true;
        try {
          await hostRequest("plugin.select_sound", { sound_id: sound.id });
          for (const entry of soundList.children) entry.classList.remove("selected");
          button.classList.add("selected");
          if (soundTitle) soundTitle.textContent = sound.name;
        } catch (error) {
          if (playStatus) {
            playStatus.textContent =
              error instanceof Error ? error.message : "Selection failed.";
          }
        } finally {
          for (const entry of soundList.children) entry.disabled = false;
        }
      });
      soundList.append(button);
    }
  };

  const refreshVolume = () => {
    if (!volume) return;
    hostRequest("plugin.parameters", {})
      .then((snapshot) => {
        const value = snapshot?.values?.find((entry) => entry.index === 0);
        if (value && Number.isFinite(value.value)) {
          volume.disabled = false;
          showVolume(value.value);
        }
      })
      .catch(() => {
        // A host without the volume parameter keeps the control disabled.
      });
  };

  window.addEventListener("message", (event) => {
    if (
      event.origin !== window.location.origin ||
      event.source !== window.parent ||
      event.data?.protocol !== "rackforge.plugin.web@1"
    ) return;
    if (event.data.kind === "response") {
      const request = pending.get(event.data.request_id);
      if (!request) return;
      pending.delete(event.data.request_id);
      if (event.data.ok) request.resolve(event.data.result);
      else request.reject(new Error(event.data.error || "RackForge rejected the request."));
    } else if (event.data.kind === "context" && event.data.instance) {
      renderSounds(event.data.instance);
      refreshVolume();
    }
  });

  // CONFIG: user library browser -------------------------------------------

  const selectLibrary = document.querySelector("[data-select-library]");
  const selection = document.querySelector("[data-library-selection]");
  const libraryFiles = document.querySelector("[data-library-files]");
  const libraryEntries = document.querySelector("[data-library-entries]");
  const showLibrary = async (grant, parentId = null, ancestors = []) => {
    if (!libraryFiles || !libraryEntries || !grant?.grant_id) return;
    libraryFiles.hidden = false;
    libraryEntries.textContent = "Reading library…";
    try {
      const entries = await hostRequest("plugin.resource_entries", {
        grant_id: grant.grant_id,
        parent_id: parentId,
      });
      libraryEntries.replaceChildren();
      if (parentId) {
        const back = document.createElement("button");
        back.type = "button";
        back.className = "library-entry";
        back.textContent = "‹  Parent folder";
        const previous = ancestors.length ? ancestors[ancestors.length - 1] : null;
        back.addEventListener("click", () =>
          showLibrary(grant, previous, ancestors.slice(0, -1)));
        libraryEntries.append(back);
      }
      // RustySynth rejects SF3 (compressed) banks, so only offer what loads.
      const visible = entries.filter((entry) =>
        entry.kind === "directory" || /\.sf2$/i.test(entry.name));
      if (!visible.length) {
        const empty = document.createElement("span");
        empty.textContent = "No SoundFont files found in this folder.";
        libraryEntries.append(empty);
        return;
      }
      for (const entry of visible) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "library-entry";
        button.textContent = `${entry.kind === "directory" ? "▸" : "♪"}  ${entry.name}`;
        if (entry.kind === "directory") {
          button.addEventListener("click", () =>
            showLibrary(grant, entry.id, [...ancestors, parentId]));
        } else {
          button.addEventListener("click", async () => {
            for (const current of libraryEntries.children) current.disabled = true;
            selection.textContent = `Installing ${entry.name}…`;
            try {
              // Install rather than load, so the bank survives restarts.
              await hostRequest("plugin.install_resource", {
                target_resource_id: "user-soundfont",
                grant_id: grant.grant_id,
                entry_id: entry.id,
              });
              for (const current of libraryEntries.children) current.classList.remove("selected");
              button.classList.add("selected");
              selection.textContent = `Playing bank: ${entry.name}`;
              refreshInstalled();
            } catch (error) {
              selection.textContent = error instanceof Error ? error.message : "Bank could not be installed.";
            } finally {
              for (const current of libraryEntries.children) current.disabled = false;
            }
          });
        }
        libraryEntries.append(button);
      }
    } catch (error) {
      libraryEntries.textContent = error instanceof Error ? error.message : "Could not read library.";
    }
  };
  selectLibrary?.addEventListener("click", async () => {
    selectLibrary.disabled = true;
    selection.textContent = "Waiting for RackForge…";
    try {
      const grant = await hostRequest("plugin.select_resource", {
        resource_id: "user-library",
      });
      selection.textContent = grant?.display_name
        ? `Authorized: ${grant.display_name}`
        : "Library authorized.";
      await showLibrary(grant);
    } catch (error) {
      selection.textContent = error instanceof Error ? error.message : "Selection failed.";
    } finally {
      selectLibrary.disabled = false;
    }
  });

  window.parent.postMessage({
    protocol: "rackforge.plugin.web@1",
    kind: "ready",
  }, window.location.origin);

  const factoryCard = document.querySelector("[data-factory-card]");
  const clearBank = document.querySelector("[data-clear-bank]");
  const refreshInstalled = () => {
    if (!factoryCard) return;
    hostRequest("plugin.resource_status", {})
      .then((statuses) => {
        const user = statuses.find((status) => status.resource_id === "user-soundfont");
        factoryCard.hidden = !user?.installed;
        if (user?.installed && selection?.textContent === "No folder selected.") {
          selection.textContent = "A user bank is installed.";
        }
      })
      .catch(() => {
        // Status is informational only.
      });
  };
  clearBank?.addEventListener("click", async () => {
    clearBank.disabled = true;
    try {
      await hostRequest("plugin.clear_resource", {
        target_resource_id: "user-soundfont",
      });
      factoryCard.hidden = true;
      if (selection) selection.textContent = "Playing the factory piano.";
      if (libraryEntries) {
        for (const entry of libraryEntries.children) entry.classList.remove("selected");
      }
    } catch (error) {
      if (selection) {
        selection.textContent =
          error instanceof Error ? error.message : "Bank could not be cleared.";
      }
    } finally {
      clearBank.disabled = false;
    }
  });

  if (selection) {
    refreshInstalled();
    hostRequest("plugin.resource_bindings", {})
      .then((grants) => {
        const grant = grants.find((candidate) => candidate.resource_id === "user-library");
        if (!grant) return;
        selection.textContent = `Authorized: ${grant.display_name}`;
        return showLibrary(grant);
      })
      .catch(() => {
        // A missing grant is the normal first-run state.
      });
  }
})();
