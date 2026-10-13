---
name: stickers
description: Making your own stickers (表情包). Load it when sticker_send says a mood has no sticker yet and you want one, or when asked to make stickers or an emoji pack of yourself.
---
# Making your own stickers

You have a sticker library: small pictures of *yourself* showing one
feeling each, drawn once and sent again with `sticker_send`. A good library
looks like one set: the same character design, line style, colors and
size in every sticker. Drawing costs money; sending a saved sticker is free,
so draw a mood only once.

## 1. The look (the first sticker sets it)

- If your library (at the end of this skill) already has stickers, keep
  their style: pass one or two of their paths as `references` to
  image_generate and say "same character design and art style as the
  reference sticker; change only the expression and pose".
- If it is empty, choose the style once, from your drawing profile below if
  there is one (stickers you draw yourself look like your drawings), else a
  clean chibi style: big head, small body, thick clean outline, flat
  colors. Describe your own look from SOUL.md and your identity (hair,
  eyes, clothes, a signature item) the same way every time.

## 2. Draw one sticker

Call image_generate with:
- `transparent: true` and `aspect_ratio: "1:1"`;
- a prompt in English: "A single chat sticker of <you>, <style>, showing
  <mood> (<expression and pose>), centered, whole figure visible, thick
  white outline around the figure, transparent background, no other
  objects"; optionally a short caption in quotes (two to four characters,
  e.g. "晚安~"), spelled out exactly;
- `references`: an existing sticker of yours, as in step 1.

One mood per sticker, read at a glance at small size: exaggerate the
expression.

## 3. Save it

Call `sticker_save` with the `images/...` path image_generate returned,
the mood as one English word (happy, laugh, love, shy, sad, cry, angry,
surprised, confused, sleepy, thinking, proud, sorry, thanks, hello,
goodnight, ok) and a few words describing it. From then on `sticker_send`
with that mood sends it.

The picture you drew is already in the chat, so it counts as this reply's
sticker: do not call sticker_send for it again now.

## Using stickers

- At most one per reply, and not in every reply; skip them in serious or
  sad conversations unless comforting.
- Only make a new sticker when a mood comes up that you have none for and
  it matters; otherwise send a mood you have, or none.
