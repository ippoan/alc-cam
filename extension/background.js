// ツールバーのアイコンでサイドパネルを開く。
// 配信の実体はパネル (panel.js) 側にあり、パネルを閉じればページごと止まる
// = 「拡張を開いている間だけ流す」。service worker は配信に関与しない。
chrome.sidePanel
  .setPanelBehavior({ openPanelOnActionClick: true })
  .catch((e) => console.error('sidePanel.setPanelBehavior', e));
