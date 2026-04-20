/* Theme toggle — shared across all pages */
(function(){
  var btn = document.getElementById('theme-toggle');
  var icon = document.getElementById('theme-icon');
  var html = document.documentElement;
  if (!btn || !icon) return;

  function getTheme(){
    return localStorage.getItem('theme') ||
      (matchMedia('(prefers-color-scheme:dark)').matches ? 'dark' : 'light');
  }
  function apply(t){
    html.setAttribute('data-theme', t);
    icon.textContent = t === 'dark' ? '\u2600' : '\u25D1';
  }
  apply(getTheme());
  btn.addEventListener('click', function(){
    var next = getTheme() === 'dark' ? 'light' : 'dark';
    localStorage.setItem('theme', next);
    apply(next);
  });
})();
