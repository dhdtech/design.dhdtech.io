;; This Source Code Form is subject to the terms of the Mozilla Public
;; License, v. 2.0. If a copy of the MPL was not distributed with this
;; file, You can obtain one at http://mozilla.org/MPL/2.0/.
;;
;; Copyright (c) KALEIDOS SUBSIDIARY SL

(ns app.common.types.shape.text
  (:require
   [app.common.schema :as sm]
   [app.common.types.fills :refer [schema:fills]]))

;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;
;; SCHEMA
;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;

(def node-types #{"root" "paragraph-set" "paragraph"})

;; Values of the Japanese layout attrs (see `app.common.types.text.japanese-layout`).
(def ^:private schema:writing-mode [:enum "horizontal-tb" "vertical-rl"])
;; Paragraphs set the orientation of the whole shape; spans carry a copy.
(def ^:private schema:text-orientation [:enum "mixed" "upright"])
(def ^:private schema:text-combine-upright [:enum "none" "all" "digits" "digits2" "digits3"])
(def ^:private schema:text-emphasis
  [:enum "none" "filled-dot" "open-dot" "filled-circle" "open-circle" "filled-sesame" "open-sesame"])
(def ^:private schema:ruby-size [:enum "half" "third" "quarter"])
(def ^:private schema:ruby-align [:enum "space-around" "center" "start" "space-between"])
(def ^:private schema:ruby-overhang [:enum "auto" "none"])
(def ^:private schema:ruby-side [:enum "over" "under"])
(def ^:private schema:warichu [:enum "none" "warichu"])
(def ^:private schema:font-features [:enum "none" "palt" "vpal"])
(def ^:private schema:annotation-clearance [:enum "none" "auto"])

(def schema:content
  [:map
   [:type [:= "root"]]
   [:key {:optional true} :string]
   [:children
    [:vector {:min 1 :gen/max 2 :gen/min 1}
     [:map
      [:type [:= "paragraph-set"]]
      [:key {:optional true} :string]
      [:children
       [:vector {:min 1 :gen/max 2 :gen/min 1}
        [:map
         [:type [:= "paragraph"]]
         [:key {:optional true} :string]
         [:fills {:optional true}
          [:maybe schema:fills]]
         [:font-family {:optional true} ::sm/text]
         [:font-size {:optional true} ::sm/text]
         [:font-style {:optional true} ::sm/text]
         [:font-weight {:optional true} ::sm/text]
         [:direction {:optional true} ::sm/text]
         [:writing-mode {:optional true} schema:writing-mode]
         [:text-orientation {:optional true} schema:text-orientation]
         [:text-decoration {:optional true} ::sm/text]
         [:text-transform {:optional true} ::sm/text]
         [:typography-ref-id {:optional true} [:maybe ::sm/uuid]]
         [:typography-ref-file {:optional true} [:maybe ::sm/uuid]]
         [:children
          [:vector {:min 1 :gen/max 2 :gen/min 1}
           [:map
            [:text :string]
            [:key {:optional true} :string]
            [:fills {:optional true}
             [:maybe schema:fills]]
            [:font-family {:optional true} ::sm/text]
            [:font-size {:optional true} ::sm/text]
            [:font-style {:optional true} ::sm/text]
            [:font-weight {:optional true} ::sm/text]
            [:direction {:optional true} ::sm/text]
            [:text-combine-upright {:optional true} schema:text-combine-upright]
            [:text-emphasis {:optional true} schema:text-emphasis]
            [:ruby {:optional true} :string]
            [:ruby-hidden {:optional true} :boolean]
            [:ruby-size {:optional true} schema:ruby-size]
            [:ruby-align {:optional true} schema:ruby-align]
            [:ruby-overhang {:optional true} schema:ruby-overhang]
            [:ruby-side {:optional true} schema:ruby-side]
            [:warichu {:optional true} schema:warichu]
            [:font-features {:optional true} schema:font-features]
            [:annotation-clearance {:optional true} schema:annotation-clearance]
            [:text-orientation {:optional true} schema:text-orientation]
            [:text-decoration {:optional true} ::sm/text]
            [:text-transform {:optional true} ::sm/text]
            [:typography-ref-id {:optional true} [:maybe ::sm/uuid]]
            [:typography-ref-file {:optional true} [:maybe ::sm/uuid]]]]]]]]]]]])

(def valid-content?
  (sm/lazy-validator schema:content))

(def schema:position-data
  [:vector {:min 0 :gen/max 2}
   [:map
    [:x ::sm/safe-number]
    [:y ::sm/safe-number]
    [:width ::sm/safe-number]
    [:height ::sm/safe-number]
    [:fills schema:fills]
    [:font-family {:optional true} ::sm/text]
    [:font-size {:optional true} ::sm/text]
    [:font-style {:optional true} ::sm/text]
    [:font-weight {:optional true} ::sm/text]
    [:rtl {:optional true} :boolean]
    [:writing-mode {:optional true} ::sm/text]
    [:text-orientation {:optional true} ::sm/text]
    [:text-combine-upright {:optional true} ::sm/text]
    [:emphasis-mark {:optional true} :boolean]
    [:ruby {:optional true} :string]
    [:ruby-size {:optional true} ::sm/text]
    [:ruby-align {:optional true} ::sm/text]
    [:ruby-overhang {:optional true} ::sm/text]
    [:ruby-side {:optional true} ::sm/text]
    [:warichu {:optional true} ::sm/text]
    [:font-features {:optional true} ::sm/text]
    [:text {:optional true} :string]
    [:text-decoration {:optional true} ::sm/text]
    [:text-transform {:optional true} ::sm/text]]])
