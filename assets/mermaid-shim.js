(function () {
  const selector = 'pre > code.language-mermaid';
  let renderSeq = 0;
  let mermaidLoaded = false;

  // Mark each block before Prism runs so highlightCodeBlocks() in
  // discuss.html can skip syntax highlighting for mermaid sources.
  function markBlocks() {
    const blocks = Array.from(document.querySelectorAll(selector)).filter(
      function (code) {
        const pre = code.parentElement;
        return pre && !pre.hasAttribute('data-mermaid');
      }
    );
    blocks.forEach(function (code) {
      const pre = code.parentElement;
      pre.classList.add('mermaid-block', 'no-line-numbers');
      pre.setAttribute('data-mermaid', 'pending');
    });
    return blocks;
  }

  function reportError(pre, error) {
    pre.setAttribute('data-mermaid', 'error');
    const note = document.createElement('div');
    note.className = 'mermaid-error';
    note.textContent =
      'mermaid render failed: ' + (error && error.message ? error.message : error);
    if (pre.parentElement) pre.parentElement.insertBefore(note, pre.nextSibling);
  }

  // Match Mermaid's built-in theme to the page theme so diagrams stay
  // legible in dark mode (light-on-dark) instead of rendering dark lines
  // and labels on the dark page background.
  function currentMermaidTheme() {
    return document.documentElement.getAttribute('data-theme') === 'dark'
      ? 'dark'
      : 'default';
  }

  function renderBlocks() {
    const mermaid = window.mermaid;
    if (!mermaid || typeof mermaid.render !== 'function') return;
    if (typeof mermaid.initialize === 'function') {
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: 'loose',
        theme: currentMermaidTheme(),
      });
    }

    document.querySelectorAll('pre[data-mermaid="pending"]').forEach(function (pre) {
      const code = pre.querySelector('code.language-mermaid');
      // Rendering replaces the <code> with the SVG, so stash the source to
      // allow a re-render when the page theme changes (see setTheme below).
      const source = code
        ? code.textContent || ''
        : pre.getAttribute('data-mermaid-src') || '';
      pre.setAttribute('data-mermaid-src', source);
      const id = 'discuss-mermaid-' + renderSeq++;
      try {
        mermaid
          .render(id, source)
          .then(function (output) {
            pre.innerHTML = output.svg;
            pre.setAttribute('data-mermaid', 'rendered');
          })
          .catch(function (error) {
            reportError(pre, error);
          });
      } catch (error) {
        reportError(pre, error);
      }
    });
  }

  let loaderStarted = false;
  function ensureLoadedThenRender() {
    if (mermaidLoaded) {
      renderBlocks();
      return;
    }
    if (loaderStarted) return; // onload will pick up pending blocks
    loaderStarted = true;
    const script = document.createElement('script');
    script.src = window.__DISCUSS_MERMAID_SRC__ || '/assets/mermaid.min.js';
    script.async = true;
    script.onload = function () {
      mermaidLoaded = true;
      renderBlocks();
    };
    document.head.appendChild(script);
  }

  // Re-scan and render after a live source update replaces #doc-content.
  window.__discussRenderMermaid = function () {
    if (markBlocks().length || document.querySelector('pre[data-mermaid="pending"]')) {
      ensureLoadedThenRender();
    }
  };

  // Re-render every diagram with the current page theme. Called by the theme
  // toggle in discuss.html — the theme is baked into each SVG at render time,
  // so switching light/dark requires rebuilding from the stashed source.
  window.__discussSetMermaidTheme = function () {
    const rendered = document.querySelectorAll(
      'pre[data-mermaid="rendered"], pre[data-mermaid="error"]'
    );
    if (!rendered.length) return;
    rendered.forEach(function (pre) {
      const note = pre.nextSibling;
      if (note && note.nodeType === 1 && note.classList.contains('mermaid-error')) {
        note.parentNode.removeChild(note);
      }
      pre.innerHTML = '';
      pre.setAttribute('data-mermaid', 'pending');
    });
    ensureLoadedThenRender();
  };

  if (markBlocks().length) ensureLoadedThenRender();
})();
