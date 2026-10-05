;; This Source Code Form is subject to the terms of the Mozilla Public
;; License, v. 2.0. If a copy of the MPL was not distributed with this
;; file, You can obtain one at http://mozilla.org/MPL/2.0/.
;;
;; Copyright (c) KALEIDOS INC Sucursal en España SL

(ns app.common.types.text.japanese-layout
  (:require
   [app.common.data.macros :as dm]
   [cuerdas.core :as str]))

;; Vertical writing (tategaki); absent means "horizontal-tb" and "mixed".
(def text-writing-mode-attrs
  [:writing-mode])

(def text-orientation-attrs
  [:text-orientation])

;; Whole-shape paragraph attrs: every paragraph carries the first one's value.
(def whole-shape-paragraph-attrs
  (into text-writing-mode-attrs text-orientation-attrs))

(def text-combine-upright-attrs
  [:text-combine-upright])

;; Emphasis mark (圏点 / bouten) applied per span; absent means no emphasis.
(def text-emphasis-attrs
  [:text-emphasis])

;; Ruby (furigana) annotation text and customization carried per span.
(def text-ruby-attrs
  [:ruby
   :ruby-hidden
   :ruby-size
   :ruby-align
   :ruby-overhang
   :ruby-side])

;; Warichu (割注): two half-size lines in one line position; "warichu" or "none".
(def text-warichu-attrs
  [:warichu])

(def text-font-features-attrs
  [:font-features])

;; "auto" adds the ruby (at its size) and emphasis marks to the line height;
;; "none" keeps the line height, so annotations sit in the line gap.
(def text-annotation-clearance-attrs
  [:annotation-clearance])

;; Span attrs owned by the span's characters, not its style: a reading or a
;; warichu note annotates one span and is never copied to other text.
(def text-content-attrs
  [:ruby :warichu])

;; Values of the span attrs above when a span does not store them.
(def span-attr-defaults
  {:text-combine-upright "none"
   :text-emphasis        "none"
   :ruby                 ""
   :ruby-hidden          false
   :ruby-size            "half"
   :ruby-align           "space-around"
   :ruby-overhang        "auto"
   :ruby-side            "over"
   :warichu              "none"
   :font-features        "none"
   :annotation-clearance "none"})

;; Glyph of each emphasis style, per CSS `text-emphasis-style`.
(def ^:private emphasis-mark-chars
  {"filled-dot"    "•"
   "open-dot"      "◦"
   "filled-circle" "●"
   "open-circle"   "○"
   "filled-sesame" "﹅"
   "open-sesame"   "﹆"})

(defn emphasis-mark-char
  "Mark glyph of a text-emphasis value, or nil for none."
  [text-emphasis]
  (get emphasis-mark-chars text-emphasis))

;; Emphasis mark font size relative to the base font size.
(def emphasis-font-scale 0.5)

;; Warichu sub-line font size relative to the base font size.
(def warichu-font-scale 0.5)

(defn ruby-font-scale
  [ruby-size]
  (case ruby-size
    "third"   (/ 1 3)
    "quarter" 0.25
    0.5))

(defn visible-ruby
  "Ruby annotation text of a text node, or nil when absent or hidden."
  [node]
  (let [ruby (:ruby node)]
    (when (and (string? ruby)
               (not (str/blank? ruby))
               (not (true? (:ruby-hidden node))))
      ruby)))

(defn annotated-span?
  "True when a text node carries a ruby reading or is a warichu note."
  [node]
  (or (not (str/blank? (:ruby node)))
      (= "warichu" (:warichu node))))

(defn- digit?
  [c]
  (let [code #?(:clj (int c) :cljs (.charCodeAt c 0))]
    (or (<= 48 code 57) (<= 0xFF10 code 0xFF19))))

(defn digit-combine-segments
  "Text split for a `digits` tate-chu-yoko value: `[text combine?]` pairs,
   where runs of two up to the value's maximum ASCII or full-width digits
   combine and longer runs stay ordinary text. Nil for other values."
  [text value]
  (when-let [max-len (case value "digits" 4 "digits2" 2 "digits3" 3 nil)]
    (->> (partition-by digit? (seq text))
         (mapv (fn [chars]
                 (let [run (apply str chars)]
                   [run (and (digit? (first chars)) (<= 2 (count chars) max-len))]))))))

(defn warichu-text?
  "True when a text node renders as warichu, which needs at least two
   characters for its two sub-lines."
  [node]
  (let [text (:text node)]
    (and (= "warichu" (:warichu node))
         (string? text)
         (>= (count text) 2))))

(defn content-writing-mode
  "Writing mode of a text content. Stored per paragraph but treated as a
   whole-shape property: the first paragraph decides the flow."
  [content]
  (dm/get-in content [:children 0 :children 0 :writing-mode]))

(defn vertical-text-content?
  "True when the text content flows vertically (vertical-rl)."
  [content]
  (= "vertical-rl" (content-writing-mode content)))
