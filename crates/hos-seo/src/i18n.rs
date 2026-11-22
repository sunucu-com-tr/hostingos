//! Default Turkish / English texts for every message key produced by
//! [`crate::geo`] and [`crate::audit`].
//!
//! Reports carry i18n *keys* plus `params`; the web layer can override texts in
//! `locales/*.json` (merge [`catalog`] as defaults) or call [`render`].
//! Placeholders use `{name}` syntax.

use std::collections::BTreeMap;

use hos_core::domain::Locale;
use serde_json::{Map, Value};

/// `(key, tr, en)`.
pub const TEXTS: &[(&str, &str, &str)] = &[
    // ---------------------------------------------------------------- GEO
    (
        "geo.direct_answer.pass",
        "Sayfa, konuyu doğrudan yanıtlayan özlü bir açılış paragrafıyla başlıyor ({words} kelime).",
        "The page opens with a concise paragraph that answers the topic directly ({words} words).",
    ),
    (
        "geo.direct_answer.warn",
        "Açılış paragrafı var ancak doğrudan yanıt olarak çok uzun ya da çok kısa ({words} kelime).",
        "There is an opening paragraph, but it is too long or too short to work as a direct answer ({words} words).",
    ),
    (
        "geo.direct_answer.fail",
        "Sayfanın başında konuyu doğrudan yanıtlayan bir paragraf yok.",
        "There is no opening paragraph that answers the page topic directly.",
    ),
    (
        "geo.direct_answer.hint",
        "H1'in hemen altına konuyu 40–60 kelimede yanıtlayan bir paragraf ekleyin; ilk cümle tek başına alıntılanabilir olsun.",
        "Add a 40–60 word paragraph right below the H1 that answers the topic; make the first sentence quotable on its own.",
    ),
    (
        "geo.question_headings.pass",
        "{count} alt başlık soru biçiminde; yapay zekâ motorları bunları soru-yanıt çifti olarak eşleyebilir.",
        "{count} subheadings are phrased as questions, which AI engines can map to question–answer pairs.",
    ),
    (
        "geo.question_headings.warn",
        "Yalnızca bir alt başlık soru biçiminde.",
        "Only one subheading is phrased as a question.",
    ),
    (
        "geo.question_headings.fail",
        "Soru biçiminde alt başlık yok.",
        "No subheadings are phrased as questions.",
    ),
    (
        "geo.question_headings.hint",
        "Kullanıcıların gerçekten sorduğu soruları H2/H3 olarak yazın (ör. \"VPS nedir?\", \"Hangi paketi seçmeliyim?\") ve hemen altında yanıtlayın.",
        "Use the questions people actually ask as H2/H3s (e.g. \"What is a VPS?\") and answer each right below it.",
    ),
    (
        "geo.faq.pass",
        "SSS bloğu ve FAQPage yapılandırılmış verisi mevcut.",
        "An FAQ block with FAQPage structured data is present.",
    ),
    (
        "geo.faq.warn",
        "SSS içeriği var ancak FAQPage yapılandırılmış verisi yok.",
        "FAQ content exists but there is no FAQPage structured data.",
    ),
    (
        "geo.faq.hidden",
        "FAQPage işaretlemesi var ancak sorularının çoğu sayfada görünmüyor ({visible}/{questions}).",
        "There is FAQPage markup, but most of its questions are not visible on the page ({visible}/{questions}).",
    ),
    (
        "geo.faq.fail",
        "Sayfada SSS (sıkça sorulan sorular) bloğu yok.",
        "The page has no FAQ block.",
    ),
    (
        "geo.faq.hint",
        "3–8 soruluk bir SSS bölümü ekleyin ve FAQPage JSON-LD ile işaretleyin; yanıtlar 2–4 cümle olsun.",
        "Add a 3–8 question FAQ section marked up with FAQPage JSON-LD; keep answers to 2–4 sentences.",
    ),
    (
        "geo.definitions.pass",
        "{count} özlü tanım cümlesi bulundu.",
        "Found {count} concise definition sentences.",
    ),
    (
        "geo.definitions.warn",
        "Yalnızca bir tanım cümlesi bulundu.",
        "Only one definition sentence was found.",
    ),
    (
        "geo.definitions.fail",
        "Özlü bir tanım cümlesi bulunamadı.",
        "No concise definition sentence was found.",
    ),
    (
        "geo.definitions.hint",
        "Ana kavramları tek cümlede tanımlayın: \"X, … sağlayan bir …dir.\" / \"X is a … that …\".",
        "Define key terms in one sentence: \"X is a … that …\".",
    ),
    (
        "geo.concrete_data.pass",
        "{count} somut veri (sayı, fiyat, ölçü) ve {tables} tablo bulundu.",
        "Found {count} concrete facts (numbers, prices, units) and {tables} tables.",
    ),
    (
        "geo.concrete_data.warn",
        "Somut veri az ({count} sayı/fiyat, {tables} tablo).",
        "Few concrete facts ({count} numbers/prices, {tables} tables).",
    ),
    (
        "geo.concrete_data.fail",
        "Somut sayı, fiyat, ölçü veya tablo yok.",
        "No concrete numbers, prices, units or tables.",
    ),
    (
        "geo.concrete_data.hint",
        "Fiyatları, kaynak limitlerini (GB, vCPU), çalışma süresi garantisini ve karşılaştırmaları sayılarla ve tablo halinde verin.",
        "State prices, resource limits (GB, vCPU), uptime guarantees and comparisons as numbers, ideally in a table.",
    ),
    (
        "geo.entity_clarity.pass",
        "Marka ve ana varlıklar başlıkta ve içerikte açıkça geçiyor.",
        "The brand and key entities are named clearly in the title and content.",
    ),
    (
        "geo.entity_clarity.warn",
        "Varlık netliği kısmi; eksik: {missing}.",
        "Entity clarity is partial; missing: {missing}.",
    ),
    (
        "geo.entity_clarity.fail",
        "Marka ve ana varlıklar başlıkta, H1'de veya girişte geçmiyor.",
        "The brand and key entities are not named in the title, H1 or introduction.",
    ),
    (
        "geo.entity_clarity.hint",
        "Marka adını, ürün adını ve hizmet verilen konumu başlıkta/girişte açıkça yazın; Organization JSON-LD'de sameAs bağlantıları verin.",
        "Name the brand, product and served location in the title/introduction; add Organization JSON-LD with sameAs links.",
    ),
    (
        "geo.freshness.pass",
        "Güncellik sinyali var (son tarih: {date}).",
        "Freshness signal present (latest date: {date}).",
    ),
    (
        "geo.freshness.warn",
        "Son tarih eski: {date} ({days} gün önce).",
        "The latest date is old: {date} ({days} days ago).",
    ),
    (
        "geo.freshness.fail",
        "Yayın veya güncelleme tarihi sinyali yok.",
        "No publication or update date signal.",
    ),
    (
        "geo.freshness.hint",
        "Görünür bir \"Son güncelleme\" tarihi, <time datetime> etiketi ve JSON-LD dateModified ekleyin; içeriği düzenli güncelleyin.",
        "Show a visible \"Last updated\" date with <time datetime> and JSON-LD dateModified; refresh content regularly.",
    ),
    (
        "geo.authorship.pass",
        "Yazar ve kaynak/güven sinyalleri mevcut (E-E-A-T).",
        "Author and source/trust signals are present (E-E-A-T).",
    ),
    (
        "geo.authorship.warn",
        "Yazar veya kaynak sinyallerinden yalnızca biri var.",
        "Only one of author or source signals is present.",
    ),
    (
        "geo.authorship.fail",
        "Yazar veya kaynak (E-E-A-T) sinyali yok.",
        "No author or source (E-E-A-T) signals.",
    ),
    (
        "geo.authorship.hint",
        "Uzmanlığı olan bir yazar adı/profil bağlantısı, güvenilir dış kaynaklara atıflar ve Hakkımızda/İletişim bağlantıları ekleyin.",
        "Add a named author with a profile link, citations to authoritative sources and About/Contact links.",
    ),
    (
        "geo.structured_data.pass",
        "Yapılandırılmış veri kapsamı iyi: {types}.",
        "Good structured data coverage: {types}.",
    ),
    (
        "geo.structured_data.warn",
        "Yapılandırılmış veri sınırlı veya hatalı: {types} ({errors} hata).",
        "Structured data is limited or has errors: {types} ({errors} errors).",
    ),
    (
        "geo.structured_data.fail",
        "Geçerli JSON-LD yapılandırılmış veri yok.",
        "No valid JSON-LD structured data.",
    ),
    (
        "geo.structured_data.hint",
        "Sayfa türüne uygun Organization, BreadcrumbList ve Product/Article/FAQPage JSON-LD ekleyin ve hatalarını giderin.",
        "Add Organization, BreadcrumbList and the page-type schema (Product/Article/FAQPage) and fix their errors.",
    ),
    (
        "geo.chunkability.pass",
        "İçerik iyi bölümlenmiş (ortalama paragraf {avg} kelime).",
        "Content is well chunked (average paragraph {avg} words).",
    ),
    (
        "geo.chunkability.warn",
        "Bazı paragraflar uzun (en uzun {max} kelime) veya başlıklar seyrek (bölüm başına {per_section} kelime).",
        "Some paragraphs are long (longest {max} words) or headings are sparse ({per_section} words per section).",
    ),
    (
        "geo.chunkability.fail",
        "İçerik uzun bloklar halinde; yapay zekâ motorlarının bölüm alıntılaması zorlaşıyor.",
        "Content comes in long blocks, which makes it hard for AI engines to quote sections.",
    ),
    (
        "geo.chunkability.hint",
        "Paragrafları 40–80 kelimede tutun, her 150–300 kelimede bir alt başlık kullanın, listeleri ve tabloları tercih edin.",
        "Keep paragraphs to 40–80 words, add a subheading every 150–300 words, prefer lists and tables.",
    ),
    (
        "geo.ssr.pass",
        "İçerik JavaScript gerektirmeden HTML'de mevcut (sunucu taraflı oluşturma).",
        "Content is present in the HTML without JavaScript (server-side rendered).",
    ),
    (
        "geo.ssr.warn",
        "HTML'de çok az görünür metin var ({words} kelime).",
        "Very little visible text in the HTML ({words} words).",
    ),
    (
        "geo.ssr.fail",
        "İçerik JavaScript ile oluşturuluyor; çoğu yapay zekâ tarayıcısı JavaScript çalıştırmaz.",
        "Content is rendered with JavaScript; most AI crawlers do not execute JavaScript.",
    ),
    (
        "geo.ssr.hint",
        "Ana içeriği sunucu tarafında HTML olarak sunun.",
        "Serve the main content as server-rendered HTML.",
    ),
    // -------------------------------------------------------------- Audit
    (
        "audit.title_missing",
        "Sayfa başlığı (<title>) yok.",
        "The page has no <title>.",
    ),
    (
        "audit.title_missing.hint",
        "Ana anahtar kelimeyi ve markayı içeren, en fazla 60 karakterlik benzersiz bir başlık yazın.",
        "Write a unique title of at most 60 characters with the main keyword and the brand.",
    ),
    (
        "audit.title_multiple",
        "Sayfada {count} adet <title> etiketi var.",
        "The page has {count} <title> tags.",
    ),
    (
        "audit.title_multiple.hint",
        "Yalnızca bir <title> bırakın.",
        "Keep a single <title>.",
    ),
    (
        "audit.title_too_short",
        "Başlık çok kısa ({length} karakter).",
        "The title is too short ({length} characters).",
    ),
    (
        "audit.title_too_short.hint",
        "Başlığı 30–60 karakter arasında, açıklayıcı tutun.",
        "Make the title descriptive, 30–60 characters.",
    ),
    (
        "audit.title_too_long",
        "Başlık çok uzun ({length}/{max} karakter); arama sonuçlarında kesilebilir.",
        "The title is too long ({length}/{max} characters) and may be truncated in results.",
    ),
    (
        "audit.title_too_long.hint",
        "Önemli kelimeleri başa alıp başlığı {max} karakterin altına indirin.",
        "Front-load key words and shorten the title below {max} characters.",
    ),
    (
        "audit.description_missing",
        "Meta açıklama yok.",
        "There is no meta description.",
    ),
    (
        "audit.description_missing.hint",
        "Sayfayı özetleyen, eyleme çağıran 120–160 karakterlik bir açıklama yazın.",
        "Write a 120–160 character description that summarises the page and invites a click.",
    ),
    (
        "audit.description_multiple",
        "Sayfada {count} meta açıklama var.",
        "The page has {count} meta descriptions.",
    ),
    (
        "audit.description_multiple.hint",
        "Yalnızca bir meta açıklama bırakın.",
        "Keep a single meta description.",
    ),
    (
        "audit.description_too_short",
        "Meta açıklama kısa ({length} karakter).",
        "The meta description is short ({length} characters).",
    ),
    (
        "audit.description_too_short.hint",
        "Açıklamayı 120–160 karaktere genişletin.",
        "Expand the description to 120–160 characters.",
    ),
    (
        "audit.description_too_long",
        "Meta açıklama uzun ({length}/{max} karakter).",
        "The meta description is long ({length}/{max} characters).",
    ),
    (
        "audit.description_too_long.hint",
        "Açıklamayı {max} karakterin altına kısaltın.",
        "Shorten the description below {max} characters.",
    ),
    (
        "audit.h1_missing",
        "Sayfada H1 başlığı yok.",
        "The page has no H1 heading.",
    ),
    (
        "audit.h1_missing.hint",
        "Sayfanın konusunu anlatan tek bir H1 ekleyin.",
        "Add a single H1 that states the page topic.",
    ),
    (
        "audit.h1_multiple",
        "Sayfada {count} adet H1 var.",
        "The page has {count} H1 headings.",
    ),
    (
        "audit.h1_multiple.hint",
        "Tek bir H1 kullanın; diğerlerini H2'ye çevirin.",
        "Use one H1 and turn the others into H2s.",
    ),
    (
        "audit.heading_skip",
        "Başlık hiyerarşisinde atlama var (H{from} → H{to}).",
        "The heading hierarchy skips a level (H{from} → H{to}).",
    ),
    (
        "audit.heading_skip.hint",
        "Başlık düzeylerini sırayla kullanın (H2'nin altında H3).",
        "Use heading levels in order (H3 under H2).",
    ),
    (
        "audit.img_alt_missing",
        "{count} görselde alt metni (alt) yok.",
        "{count} images have no alt attribute.",
    ),
    (
        "audit.img_alt_missing.hint",
        "Görseli anlatan kısa alt metinler ekleyin; dekoratif görsellerde alt=\"\" kullanın.",
        "Add short descriptive alt text; use alt=\"\" for decorative images.",
    ),
    (
        "audit.canonical_missing",
        "Canonical bağlantısı yok.",
        "There is no canonical link.",
    ),
    (
        "audit.canonical_missing.hint",
        "Sayfanın tercih edilen mutlak URL'sini rel=canonical ile belirtin.",
        "Declare the preferred absolute URL with rel=canonical.",
    ),
    (
        "audit.canonical_multiple",
        "Sayfada {count} canonical bağlantısı var.",
        "The page has {count} canonical links.",
    ),
    (
        "audit.canonical_multiple.hint",
        "Yalnızca bir canonical bırakın; arama motorları çelişkili canonical'ları yok sayar.",
        "Keep one canonical; search engines ignore conflicting canonicals.",
    ),
    (
        "audit.canonical_invalid",
        "Canonical URL geçersiz: {url}",
        "The canonical URL is invalid: {url}",
    ),
    (
        "audit.canonical_invalid.hint",
        "Canonical için mutlak http(s) URL kullanın.",
        "Use an absolute http(s) URL as canonical.",
    ),
    (
        "audit.canonical_relative",
        "Canonical göreli bir URL: {url}",
        "The canonical is a relative URL: {url}",
    ),
    (
        "audit.canonical_relative.hint",
        "Canonical'ı mutlak URL olarak yazın.",
        "Write the canonical as an absolute URL.",
    ),
    (
        "audit.canonical_other",
        "Sayfa başka bir URL'yi canonical gösteriyor: {url}",
        "The page declares another URL as canonical: {url}",
    ),
    (
        "audit.canonical_other.hint",
        "Bu bilinçli değilse canonical'ı sayfanın kendi URL'sine çevirin.",
        "If unintended, point the canonical to the page's own URL.",
    ),
    (
        "audit.hreflang_invalid",
        "Geçersiz hreflang kodları: {codes}",
        "Invalid hreflang codes: {codes}",
    ),
    (
        "audit.hreflang_invalid.hint",
        "ISO 639-1 dil (ve isteğe bağlı ISO 3166-1 bölge) kodu kullanın: tr, en-GB, x-default.",
        "Use ISO 639-1 language (and optional ISO 3166-1 region) codes: tr, en-GB, x-default.",
    ),
    (
        "audit.hreflang_no_self",
        "hreflang listesinde sayfanın kendisi yok.",
        "The hreflang set does not include the page itself.",
    ),
    (
        "audit.hreflang_no_self.hint",
        "Her dil sürümü kendisini de hreflang listesinde göstermelidir.",
        "Every language version must list itself in its hreflang set.",
    ),
    (
        "audit.hreflang_no_x_default",
        "hreflang x-default tanımlı değil.",
        "No hreflang x-default is defined.",
    ),
    (
        "audit.hreflang_no_x_default.hint",
        "Dil seçimi yapılmamış kullanıcılar için x-default ekleyin.",
        "Add x-default for users whose language is not matched.",
    ),
    (
        "audit.noindex",
        "Sayfa noindex; arama sonuçlarında ve yapay zekâ yanıtlarında görünmeyecek.",
        "The page is noindex and will not appear in search results or AI answers.",
    ),
    (
        "audit.noindex.hint",
        "Bu sayfa dizinlenmeliyse noindex yönergesini kaldırın.",
        "Remove the noindex directive if the page should be indexed.",
    ),
    (
        "audit.noindex_canonical_conflict",
        "noindex ile başka bir URL'ye canonical birlikte kullanılmış (çelişkili sinyal).",
        "noindex is combined with a canonical to another URL (conflicting signals).",
    ),
    (
        "audit.noindex_canonical_conflict.hint",
        "Ya canonical ya da noindex kullanın, ikisini birden değil.",
        "Use either a canonical or noindex, not both.",
    ),
    (
        "audit.robots_conflict",
        "robots meta etiketleri hem index hem noindex içeriyor.",
        "Robots meta tags contain both index and noindex.",
    ),
    (
        "audit.robots_conflict.hint",
        "Tek ve tutarlı bir robots yönergesi bırakın.",
        "Keep a single consistent robots directive.",
    ),
    (
        "audit.nofollow",
        "Sayfa nofollow; bağlantılarının değeri aktarılmayacak.",
        "The page is nofollow; its links will not pass value.",
    ),
    (
        "audit.nofollow.hint",
        "Gerekmiyorsa nofollow yönergesini kaldırın.",
        "Remove nofollow unless required.",
    ),
    (
        "audit.links_internal_none",
        "Sayfada dahili bağlantı yok.",
        "The page has no internal links.",
    ),
    (
        "audit.links_internal_none.hint",
        "İlgili paketlere, SSS ve bilgi bankası makalelerine bağlantı verin.",
        "Link to related packages, FAQs and knowledge-base articles.",
    ),
    (
        "audit.link_empty_anchor",
        "{count} bağlantının metni boş.",
        "{count} links have empty anchor text.",
    ),
    (
        "audit.link_empty_anchor.hint",
        "Bağlantılara açıklayıcı metin veya aria-label ekleyin.",
        "Give links descriptive text or an aria-label.",
    ),
    (
        "audit.link_javascript",
        "{count} bağlantı javascript: veya # kullanıyor.",
        "{count} links use javascript: or #.",
    ),
    (
        "audit.link_javascript.hint",
        "Gerçek URL'lere bağlantı verin; etkileşimler için <button> kullanın.",
        "Link to real URLs; use <button> for interactions.",
    ),
    (
        "audit.thin_content_severe",
        "İçerik çok az ({words} kelime).",
        "Very little content ({words} words).",
    ),
    (
        "audit.thin_content_severe.hint",
        "Sayfaya kullanıcının sorusunu tam yanıtlayan özgün içerik ekleyin.",
        "Add original content that fully answers the visitor's question.",
    ),
    (
        "audit.thin_content",
        "İçerik ince ({words} kelime, önerilen en az {min}).",
        "Thin content ({words} words, at least {min} recommended).",
    ),
    (
        "audit.thin_content.hint",
        "Özellikleri, kullanım senaryolarını ve SSS'yi ekleyerek içeriği zenginleştirin.",
        "Enrich the content with features, use cases and FAQs.",
    ),
    (
        "audit.jsonld_invalid",
        "{count} JSON-LD bloğu ayrıştırılamıyor.",
        "{count} JSON-LD blocks cannot be parsed.",
    ),
    (
        "audit.jsonld_invalid.hint",
        "JSON sözdizimini düzeltin (virgül, tırnak, kaçış).",
        "Fix the JSON syntax (commas, quotes, escaping).",
    ),
    (
        "audit.jsonld_errors",
        "Yapılandırılmış veride {count} hata var: {details}",
        "Structured data has {count} errors: {details}",
    ),
    (
        "audit.jsonld_errors.hint",
        "Zorunlu alanları ekleyin; Google Zengin Sonuçlar Testi ile doğrulayın.",
        "Add the required properties and verify with Google's Rich Results Test.",
    ),
    (
        "audit.jsonld_warnings",
        "Yapılandırılmış veride {count} önerilen alan eksik: {details}",
        "Structured data misses {count} recommended properties: {details}",
    ),
    (
        "audit.jsonld_warnings.hint",
        "Önerilen alanlar zengin sonuç olasılığını artırır.",
        "Recommended properties improve rich-result eligibility.",
    ),
    (
        "audit.jsonld_missing",
        "Sayfada yapılandırılmış veri (JSON-LD) yok.",
        "The page has no structured data (JSON-LD).",
    ),
    (
        "audit.jsonld_missing.hint",
        "Sayfa türüne uygun schema.org işaretlemesi ekleyin.",
        "Add schema.org markup that fits the page type.",
    ),
    (
        "audit.og_missing",
        "Eksik Open Graph etiketleri: {tags}",
        "Missing Open Graph tags: {tags}",
    ),
    (
        "audit.og_missing.hint",
        "Paylaşım önizlemeleri için og:title, og:description, og:image ve og:url ekleyin.",
        "Add og:title, og:description, og:image and og:url for share previews.",
    ),
    (
        "audit.twitter_missing",
        "twitter:card etiketi yok.",
        "There is no twitter:card tag.",
    ),
    (
        "audit.twitter_missing.hint",
        "twitter:card ekleyin (görsel varsa summary_large_image).",
        "Add twitter:card (summary_large_image when there is an image).",
    ),
    (
        "audit.keyword_title",
        "Odak anahtar kelime \"{keyword}\" başlıkta geçmiyor.",
        "Focus keyword \"{keyword}\" is not in the title.",
    ),
    (
        "audit.keyword_title.hint",
        "Anahtar kelimeyi başlığın başına yakın kullanın.",
        "Use the keyword near the start of the title.",
    ),
    (
        "audit.keyword_h1",
        "Odak anahtar kelime \"{keyword}\" H1'de geçmiyor.",
        "Focus keyword \"{keyword}\" is not in the H1.",
    ),
    (
        "audit.keyword_h1.hint",
        "H1'de anahtar kelimeyi doğal biçimde kullanın.",
        "Use the keyword naturally in the H1.",
    ),
    (
        "audit.keyword_intro",
        "Odak anahtar kelime \"{keyword}\" ilk paragrafta geçmiyor.",
        "Focus keyword \"{keyword}\" is not in the first paragraph.",
    ),
    (
        "audit.keyword_intro.hint",
        "Anahtar kelimeyi giriş paragrafında kullanın.",
        "Mention the keyword in the introduction.",
    ),
    (
        "audit.keyword_description",
        "Odak anahtar kelime \"{keyword}\" meta açıklamada geçmiyor.",
        "Focus keyword \"{keyword}\" is not in the meta description.",
    ),
    (
        "audit.keyword_description.hint",
        "Meta açıklamada anahtar kelimeyi kullanın; arama sonuçlarında kalın gösterilir.",
        "Use the keyword in the description; it is shown in bold in results.",
    ),
    (
        "audit.keyword_url",
        "Odak anahtar kelime \"{keyword}\" URL'de geçmiyor.",
        "Focus keyword \"{keyword}\" is not in the URL.",
    ),
    (
        "audit.keyword_url.hint",
        "Kısa, anahtar kelimeli bir URL kısa adı (slug) kullanın.",
        "Use a short slug that contains the keyword.",
    ),
    (
        "audit.keyword_stuffing",
        "Anahtar kelime yoğunluğu yüksek (%{density}).",
        "Keyword density is high ({density}%).",
    ),
    (
        "audit.keyword_stuffing.hint",
        "Tekrarları azaltın; eş anlamlılar ve ilgili terimler kullanın.",
        "Reduce repetition; use synonyms and related terms.",
    ),
    (
        "audit.readability_low",
        "Okunabilirlik düşük (puan {score}).",
        "Readability is low (score {score}).",
    ),
    (
        "audit.readability_low.hint",
        "Cümleleri kısaltın, teknik terimleri açıklayın.",
        "Shorten sentences and explain technical terms.",
    ),
    (
        "audit.readability_very_low",
        "Okunabilirlik çok düşük (puan {score}).",
        "Readability is very low (score {score}).",
    ),
    (
        "audit.readability_very_low.hint",
        "Uzun cümleleri bölün; paragraf başına tek fikir anlatın.",
        "Split long sentences; one idea per paragraph.",
    ),
    (
        "audit.lang_missing",
        "<html> etiketinde lang özniteliği yok.",
        "The <html> element has no lang attribute.",
    ),
    (
        "audit.lang_missing.hint",
        "<html lang=\"tr\"> gibi bir dil kodu ekleyin.",
        "Add a language code such as <html lang=\"en\">.",
    ),
    (
        "audit.viewport_missing",
        "viewport meta etiketi yok (mobil uyumluluk).",
        "There is no viewport meta tag (mobile friendliness).",
    ),
    (
        "audit.viewport_missing.hint",
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"> ekleyin.",
        "Add <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">.",
    ),
    // ---------------------------------------------------------- Site-wide
    (
        "audit.duplicate_title",
        "{count} sayfa aynı başlığı kullanıyor: \"{title}\"",
        "{count} pages share the same title: \"{title}\"",
    ),
    (
        "audit.duplicate_title.hint",
        "Her sayfaya benzersiz bir başlık verin.",
        "Give every page a unique title.",
    ),
    (
        "audit.duplicate_description",
        "{count} sayfa aynı meta açıklamayı kullanıyor.",
        "{count} pages share the same meta description.",
    ),
    (
        "audit.duplicate_description.hint",
        "Her sayfa için benzersiz bir açıklama yazın.",
        "Write a unique description for every page.",
    ),
    (
        "audit.hreflang_not_reciprocal",
        "{from} sayfası {to} sayfasını hreflang ile gösteriyor ancak karşılığı yok.",
        "{from} lists {to} as an hreflang alternate, but {to} does not link back.",
    ),
    (
        "audit.hreflang_not_reciprocal.hint",
        "hreflang bağlantıları karşılıklı olmalı; her sürüm diğerlerini listelemelidir.",
        "hreflang links must be reciprocal; every version must list the others.",
    ),
    (
        "audit.canonical_to_noindex",
        "{from} sayfasının canonical'ı noindex olan {to} sayfasını gösteriyor.",
        "{from} points its canonical to {to}, which is noindex.",
    ),
    (
        "audit.canonical_to_noindex.hint",
        "Canonical hedefinin dizinlenebilir olduğundan emin olun.",
        "Make sure canonical targets are indexable.",
    ),
];

/// Default text for `key` in `locale`.
pub fn lookup(key: &str, locale: Locale) -> Option<&'static str> {
    TEXTS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, tr, en)| match locale {
            Locale::Tr => *tr,
            Locale::En => *en,
        })
}

/// Renders `key` with `{name}` placeholders replaced from `params`; falls
/// back to the key itself when unknown.
pub fn render(key: &str, locale: Locale, params: &BTreeMap<String, String>) -> String {
    let template = lookup(key, locale).unwrap_or(key);
    interpolate(template, params)
}

/// Replaces `{name}` placeholders.
pub fn interpolate(template: &str, params: &BTreeMap<String, String>) -> String {
    let mut out = template.to_owned();
    for (k, v) in params {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// All default texts of a locale as a flat `{key: text}` JSON object, for
/// merging into `locales/{tr,en}.json`.
pub fn catalog(locale: Locale) -> Value {
    let mut map = Map::new();
    for (k, tr, en) in TEXTS {
        let text = match locale {
            Locale::Tr => tr,
            Locale::En => en,
        };
        map.insert((*k).into(), Value::String((*text).into()));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_unique_and_placeholders_match() {
        let mut seen = std::collections::HashSet::new();
        let ph = |s: &str| {
            let mut v: Vec<String> = s
                .split('{')
                .skip(1)
                .filter_map(|p| p.split_once('}').map(|(n, _)| n.to_owned()))
                .collect();
            v.sort();
            v.dedup();
            v
        };
        for (k, tr, en) in TEXTS {
            assert!(seen.insert(*k), "duplicate {k}");
            assert_eq!(ph(tr), ph(en), "placeholder mismatch in {k}");
        }
    }

    #[test]
    fn render_interpolates() {
        let mut p = BTreeMap::new();
        p.insert("count".to_owned(), "3".to_owned());
        assert_eq!(
            render("audit.h1_multiple", Locale::En, &p),
            "The page has 3 H1 headings."
        );
        assert_eq!(
            render("audit.h1_multiple", Locale::Tr, &p),
            "Sayfada 3 adet H1 var."
        );
        assert_eq!(render("unknown.key", Locale::Tr, &p), "unknown.key");
        assert_eq!(
            catalog(Locale::En)["geo.faq.fail"],
            "The page has no FAQ block."
        );
    }
}
