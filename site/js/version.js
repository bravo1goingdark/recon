// Fetch latest.json and patch all [data-version] elements.
fetch("/latest.json")
  .then(r => r.ok ? r.json() : null)
  .then(d => {
    if (!d || !d.version) return;
    const v = d.version.startsWith("v") ? d.version : "v" + d.version;
    document.querySelectorAll("[data-version]").forEach(el => { el.textContent = v; });
  })
  .catch(() => {});
