/* Active TOC + sidebar highlight on scroll for /Docs.html */
(function () {
  var ids = [
    "introduction",
    "install",
    "configuration",
    "shapes",
    "tiers",
    "incremental",
    "filter-dsl",
    "tools",
    "security",
    "performance",
    "adrs",
    "changelog",
    "license",
  ];
  var links = ids.map(function (id) {
    return {
      id: id,
      toc: document.querySelector('aside.toc a[href="#' + id + '"]'),
      side: document.querySelector('aside.side a[href="#' + id + '"]'),
    };
  });
  var io = new IntersectionObserver(
    function (entries) {
      entries.forEach(function (e) {
        if (!e.isIntersecting) return;
        links.forEach(function (l) {
          if (l.toc) l.toc.classList.remove("on");
          if (l.side) l.side.classList.remove("on");
        });
        var active = links.find(function (l) {
          return l.id === e.target.id;
        });
        if (active && active.toc) active.toc.classList.add("on");
        if (active && active.side) active.side.classList.add("on");
      });
    },
    { rootMargin: "-96px 0px -70% 0px", threshold: 0 },
  );
  ids.forEach(function (id) {
    var el = document.getElementById(id);
    if (el) io.observe(el);
  });
})();
