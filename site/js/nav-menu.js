// Mobile nav-sheet toggle. CSP forbids inline scripts (script-src 'self'),
// so the burger / sheet behaviour lives here.
(function(){
  var burger = document.getElementById('nav-burger');
  var sheet = document.getElementById('nav-sheet');
  if (!burger || !sheet) return;

  function setOpen(open){
    burger.setAttribute('aria-expanded', open ? 'true' : 'false');
    burger.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
    sheet.classList.toggle('open', open);
    document.body.classList.toggle('nav-open', open);
  }

  burger.addEventListener('click', function(){
    setOpen(burger.getAttribute('aria-expanded') !== 'true');
  });

  // Tapping any link inside the sheet closes it before navigating —
  // both same-page anchors and cross-page links should leave the sheet
  // closed so the next view doesn't paint behind a stale overlay.
  sheet.addEventListener('click', function(e){
    var t = e.target;
    while (t && t !== sheet) {
      if (t.tagName === 'A') { setOpen(false); return; }
      t = t.parentNode;
    }
  });

  document.addEventListener('keydown', function(e){
    if (e.key === 'Escape' && burger.getAttribute('aria-expanded') === 'true') {
      setOpen(false);
    }
  });

  // Crossing the breakpoint while the sheet is open (e.g. rotating from
  // portrait to landscape on a tablet) leaves an invisible-but-active
  // overlay catching clicks. Reset on resize past 720px.
  var mq = window.matchMedia('(min-width: 721px)');
  function onMq(){ if (mq.matches) setOpen(false); }
  if (mq.addEventListener) mq.addEventListener('change', onMq);
  else if (mq.addListener) mq.addListener(onMq);
})();
