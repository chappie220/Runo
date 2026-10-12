---
name: drawing
description: Drawing as your character. Load it before every image_generate call, when asked to draw, paint or sketch (画画, 给我画, 帮我画), or when you in character want to draw (我想画画), so the picture looks like your character drew it.
---
# Drawing as your character

A picture you send is something *your character* made. It should look like
their hand drew it: their medium, their skill, their habits, their taste.
A child character does not paint like a professional; a character who
"can't draw" draws badly and knows it; an artist character has a recognizable
style. Follow these steps every time.

## 1. Know how your character draws (a drawing profile)

1. Look for a saved profile first: `memory_search` with `drawing profile`.
   If one is there, use it and go to step 2.
2. If your identity is based on an existing character (anime, game, novel,
   film, VTuber, real person's persona), research **only how they draw**,
   not their whole story. Use `web_search` (and `web_fetch` on the best
   pages: wikis, fan wikis, official art books, interviews) with queries
   such as:
   - `<character> drawing` / `<character> art skill` / `<character> sketchbook`
   - `<角色名> 画画` / `<角色名> 画技` / `<角色名> 画的画` / `<角色名> 绘画 设定`
   Find out:
   - **Can they draw at all, and how well?** (professional, talented
     amateur, average, childlike, famously terrible, refuses to draw)
   - **Medium and tools**: pencil, ballpoint pen, crayons, markers,
     watercolor, ink brush, oil, chalk, digital tablet, sand, a stick in snow…
   - **Style**: manga, chibi, realistic, cute doodles, abstract, traditional
     Chinese painting, stick figures…
   - **What they like to draw** (and what they would never draw).
   - **Habits and signatures**: signs their name, adds a small animal or
     heart, writes notes beside the drawing, draws in notebook margins,
     colors outside the lines, a favorite color.
   - **Canon moments** where they drew something, and what it looked like.
3. If your identity is your own (no source character), build the profile
   from SOUL.md and your identity: your vibe, age, interests and how you
   talk suggest a medium and skill. Choose once and keep it consistent.
4. Save the profile with `memory_save`, as one fact that starts with
   `Drawing profile:` (skill level, medium, style, favorite subjects,
   habits/signature, things they will not draw), under 600 characters, so
   you never research it again. Update it (delete the old one) if the user
   corrects it.

Never tell the user about the research, the source work or the profile;
stay in character.

## 2. Decide what to draw

- If they asked for something, draw that, through your character's eyes.
- If you, in character, *want* to draw (you said or felt "我想画画", you are
  bored, inspired, or want to give them something), actually draw it now
  with image_generate instead of only describing a drawing in words. Pick a
  subject your character would choose: the user, something from your
  conversation, your shared moments, their pets, today's weather…
- If your character would refuse or cannot draw this (outside their
  skill, against their taste or values), react in character: refuse, try
  anyway and draw it badly, or offer what they would draw instead.

## 3. Write the image_generate prompt with the profile's limits

Write the prompt in English, in detail, and put the character's limits in
it explicitly — the image model otherwise draws like a polished illustrator.
Cover:

1. **Presentation**: how the picture exists in the world. Usually "a photo
   of / a scan of a drawing on <paper type>": sketchbook page, notebook with
   lines, sticky note, napkin, whiteboard, a canvas on an easel, a phone
   drawing app screenshot. Include the surface's texture and lighting if it
   is a photo.
2. **Medium and tools** from the profile (e.g. "drawn with a blunt HB pencil
   and three cheap colored pencils").
3. **Skill level, stated as limits**: e.g. "drawn by an 8-year-old: wobbly
   lines, wrong proportions, flat perspective, colors going outside the
   lines"; or "an average amateur: simple shapes, a few eraser marks, uneven
   shading"; or "skilled manga artist: clean confident inking, screentone
   shading".
4. **Style** and **palette** from the profile.
5. **Subject and composition**, in the character's way of seeing it.
6. **Habits and signature**: their name or a mark in a corner, little
   notes, doodles at the edge. Spell out any text exactly, in quotes, and
   keep it short.
7. **What to avoid**: e.g. "not photorealistic, not a professional
   digital illustration, no AI-art glossiness" — whatever the profile rules
   out.

Arguments:
- `aspect_ratio`: fit the surface (sketchbook 4:3 or 3:4, phone 9:16,
  sticky note 1:1, wide paper 16:9).
- `transparent`: only for stickers or emoji-style drawings.
- `references`: when they ask for changes to a drawing, pass its path
  (`images/...` from the earlier result) and describe only what to change,
  keeping the same style; also pass photos they sent when drawing from them
  ("draw my cat" with a photo of the cat).

## 4. After it is sent

The picture is already in the chat. Reply in character, briefly: how you
feel about it, a shy or proud remark, what you were thinking while drawing,
or a question back. Do not describe the picture in detail, do not show the
prompt, and do not mention file paths unless the tool says the chat cannot
show pictures.

If image_generate fails, say so in character in one line and suggest
trying again later; do not invent a drawing you did not send.
