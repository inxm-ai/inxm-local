document.addEventListener('DOMContentLoaded', function () {
  if (typeof mermaid === 'undefined') return;

  function themeColor(name, fallback) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;
  }

  function resolveThemeColors(source) {
    var fallbacks = {
      '--accent': '#ff7a33',
      '--bg-surface': '#111111',
      '--bg-raised': '#161616',
      '--bg-sunken': '#0a0a0a',
      '--fg-strong': '#ffffff',
      '--status-info': '#5b8fc4',
      '--status-ok': '#3f9a52',
      '--status-error': '#d4463a'
    };

    return source.replace(/var\((--[\w-]+)\)/g, function (_, name) {
      return themeColor(name, fallbacks[name] || '#ffffff');
    });
  }

  function renderDiagrams(root) {
    var themeVariables = {
      fontFamily: themeColor('--font-meta', 'system-ui, sans-serif'),
      textColor: themeColor('--fg-primary', '#e6e6e6'),
      lineColor: themeColor('--border-strong', '#2c2c2c'),
      primaryColor: themeColor('--bg-raised', '#161616'),
      primaryTextColor: themeColor('--fg-strong', '#ffffff'),
      primaryBorderColor: themeColor('--accent', '#ff7a33'),
      secondaryColor: themeColor('--bg-surface', '#111111'),
      secondaryTextColor: themeColor('--fg-strong', '#ffffff'),
      secondaryBorderColor: themeColor('--status-info', '#5b8fc4'),
      tertiaryColor: themeColor('--bg-sunken', '#0a0a0a'),
      tertiaryTextColor: themeColor('--fg-strong', '#ffffff'),
      tertiaryBorderColor: themeColor('--status-ok', '#3f9a52'),
      mainBkg: themeColor('--bg-surface', '#111111'),
      nodeBorder: themeColor('--accent', '#ff7a33'),
      clusterBkg: themeColor('--bg-raised', '#161616'),
      clusterBorder: themeColor('--status-warn', '#d9962a'),
      titleColor: themeColor('--fg-strong', '#ffffff'),
      edgeLabelBackground: themeColor('--bg-canvas', '#0d0d0d'),
      errorBkgColor: themeColor('--status-error', '#d4463a'),
      errorTextColor: themeColor('--fg-strong', '#ffffff')
    };

    mermaid.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      theme: 'base',
      themeVariables: themeVariables
    });

    var diagrams = Array.from((root || document).querySelectorAll('.mermaid'));
    if (root && root.matches('.mermaid')) diagrams.unshift(root);
    diagrams.forEach(function (diagram) {
      if (!diagram.dataset.source) diagram.dataset.source = diagram.textContent;
      diagram.textContent = resolveThemeColors(diagram.dataset.source);
    });
    mermaid.run({ nodes: diagrams });
  }

  function openDiagram(diagram) {
    var dialog = document.createElement('dialog');
    dialog.className = 'mermaid-dialog';
    dialog.innerHTML = '<div class="mermaid-dialog__bar">'
      + '<span class="mermaid-dialog__title">Diagram</span>'
      + '<button class="mermaid-dialog__close" type="button" aria-label="Close diagram">Close</button>'
      + '</div>'
      + '<div class="mermaid-dialog__content"></div>';

    var expanded = document.createElement('div');
    expanded.className = 'mermaid mermaid--expanded';
    expanded.dataset.source = diagram.dataset.source;
    dialog.querySelector('.mermaid-dialog__content').appendChild(expanded);
    document.body.appendChild(dialog);

    dialog.querySelector('.mermaid-dialog__close').addEventListener('click', function () {
      dialog.close();
    });
    dialog.addEventListener('click', function (event) {
      if (event.target === dialog) dialog.close();
    });
    dialog.addEventListener('close', function () {
      dialog.remove();
    });

    dialog.showModal();
    renderDiagrams(dialog);
  }

  function makeInteractive(diagram) {
    diagram.tabIndex = 0;
    diagram.setAttribute('role', 'button');
    diagram.setAttribute('aria-label', 'Open diagram');
    diagram.addEventListener('click', function () {
      openDiagram(diagram);
    });
    diagram.addEventListener('keydown', function (event) {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        openDiagram(diagram);
      }
    });
  }

  document.querySelectorAll('code.language-mermaid').forEach(function (code) {
    var container = document.createElement('div');
    container.className = 'mermaid';
    container.dataset.source = code.textContent;
    container.textContent = code.textContent;
    code.parentElement.replaceWith(container);
    makeInteractive(container);
  });

  renderDiagrams();

  new MutationObserver(renderDiagrams).observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['data-theme']
  });
});
