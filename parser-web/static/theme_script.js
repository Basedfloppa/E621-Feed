(() => {
	var theme = null;
	try {
		theme = localStorage.getItem("theme");
	} catch (_) {}
	if (!theme) {
		theme = window.matchMedia("(prefers-color-scheme: dark)").matches
			? "dark"
			: "light";
	}
	document.documentElement.setAttribute("data-bs-theme", theme);
})();
