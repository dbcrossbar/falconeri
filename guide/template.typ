#set text(
  lang: "en",
  font: (
    "Helvetica Neue",
    "Helvetica",
    "Arial",
  ),
)

#show raw: set text(font: (
  "Menlo",
  "Monaco",
  "Consolas",
  "DejaVu Sans Mono",
))

#show link: underline

#show raw.where(block: true): block.with(
  width: 100%,
  fill: luma(240),
  inset: 10pt,
  radius: 4pt,
)

#show quote.where(block: true): block.with(
  width: 100%,
  fill: rgb("#f1f6f9"),
  inset: 10pt,
  radius: 4pt,
)

#set page(
  header: context {
    if counter(page).get().first() > 1 [
      _falconeri guide_
    ]
  },
  footer: context {
    if counter(page).get().first() > 1 [
      #counter(page).display(
        "1/1",
        both: true,
      )
    ]
  },
)

#align(center)[
  #text(22pt, weight: "bold")[falconeri]
  #v(0.5em)
  #text(14pt)[Distributed batch processing using Kubernetes]
]

#pagebreak()
#outline(depth: 2, indent: 1em, title: "Contents")
#pagebreak()

/**** MDBOOK_TYPST_PDF_PLACEHOLDER ****/
