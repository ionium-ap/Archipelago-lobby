// A selector of Archipelago versions that isn't part of a form: choosing a version loads the page
// again for it, as its `base` parameter. Everything else in the address stays.
for (const select of document.querySelectorAll("select[data-base-select]")) {
    select.addEventListener("change", () => {
        const url = new URL(window.location.href);
        url.searchParams.set("base", select.value);
        window.location.href = url.toString();
    });
}
