//! Comments under the video you're watching: folded away until asked for,
//! read only. Replies open in place under their comment.

use linen_core::yt::{Comment, Comments};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{App, CommentItem, Msg, State, Watch};

/// What a comments request was for.
#[derive(Debug, Clone)]
pub enum Ask {
    /// the first page (true: newest first)
    First(bool),
    /// the next page
    More,
    /// a comment's replies (its id)
    Replies(String),
    /// more replies of this comment
    MoreReplies(String),
}

/// One line of the list.
#[derive(Debug, Clone)]
pub enum Row {
    Comment { c: Comment, open: bool, loading: bool },
    /// "Show more replies" (for the comment with this id; where they are)
    MoreReplies { parent: String, token: String, loading: bool },
}

#[derive(Default)]
pub struct Thread {
    pub video: String,
    pub count: String,
    pub top: Option<String>,
    pub newest: Option<String>,
    pub rows: Vec<Row>,
    pub more: Option<String>,
    pub busy: bool,
    pub newest_first: bool,
}

impl State {
    /// A new video on the Watch page: its comments start folded.
    pub fn reset_comments(&mut self, ui: &App, id: &str) {
        self.thread = Thread { video: id.to_string(), ..Default::default() };
        let w = ui.global::<Watch>();
        w.set_comments_shown(false);
        w.set_comments_sort(0);
        w.set_comment_count("".into());
        w.set_comments_msg("".into());
        self.show_comments(ui);
    }

    /// From the video's details: how many, and where they start.
    pub fn comments_found(&mut self, ui: &App, id: &str, count: &str, top: Option<String>, newest: Option<String>) {
        if self.thread.video != id {
            return;
        }
        self.thread.count = count.to_string();
        self.thread.top = top;
        self.thread.newest = newest;
        let w = ui.global::<Watch>();
        w.set_comment_count(count.into());
        w.set_has_comments(self.thread.top.is_some());
        // developer aid: LINEN_COMMENTS=1 opens them (screenshots)
        if std::env::var("LINEN_COMMENTS").is_ok_and(|v| v == "1") {
            w.set_comments_shown(true);
        }
        // opened before the details came: load now
        if w.get_comments_shown() && self.thread.rows.is_empty() && !self.thread.busy {
            self.load_comments(ui, Ask::First(self.thread.newest_first));
        }
    }

    pub fn open_comments(&mut self, ui: &App) {
        ui.global::<Watch>().set_comments_shown(true);
        if self.thread.rows.is_empty() && !self.thread.busy {
            self.load_comments(ui, Ask::First(false));
        }
    }

    pub fn sort_comments(&mut self, ui: &App, newest: bool) {
        if newest == self.thread.newest_first && !self.thread.rows.is_empty() {
            return;
        }
        ui.global::<Watch>().set_comments_sort(newest as i32);
        self.load_comments(ui, Ask::First(newest));
    }

    /// Open or close a comment's replies.
    pub fn toggle_replies(&mut self, ui: &App, id: &str) {
        let Some(i) = self.comment_row(id) else { return };
        let Row::Comment { c, open, loading } = &mut self.thread.rows[i] else { return };
        if *loading {
            return;
        }
        if *open {
            *open = false;
            // take its replies (and their "more") out
            let end = (i + 1..self.thread.rows.len()).find(|&j| self.is_top(j)).unwrap_or(self.thread.rows.len());
            self.thread.rows.drain(i + 1..end);
            self.show_comments(ui);
            return;
        }
        if c.replies_token.is_none() {
            return;
        }
        *loading = true;
        self.load_comments(ui, Ask::Replies(id.to_string()));
    }

    pub fn more_replies(&mut self, ui: &App, parent: &str) {
        let found = self.thread.rows.iter_mut().find(|r| matches!(r, Row::MoreReplies { parent: p, .. } if p == parent));
        if let Some(Row::MoreReplies { loading, .. }) = found {
            if *loading {
                return;
            }
            *loading = true;
            self.load_comments(ui, Ask::MoreReplies(parent.to_string()));
        }
    }

    pub fn more_comments(&mut self, ui: &App) {
        if self.thread.more.is_some() && !self.thread.busy {
            self.load_comments(ui, Ask::More);
        }
    }

    fn comment_row(&self, id: &str) -> Option<usize> {
        self.thread.rows.iter().position(|r| matches!(r, Row::Comment { c, .. } if c.id == id))
    }

    fn is_top(&self, i: usize) -> bool {
        matches!(&self.thread.rows[i], Row::Comment { c, .. } if !c.reply)
    }

    fn load_comments(&mut self, ui: &App, ask: Ask) {
        let token = match &ask {
            Ask::First(newest) => if *newest { self.thread.newest.clone() } else { self.thread.top.clone() },
            Ask::More => self.thread.more.clone(),
            Ask::Replies(id) => self.comment_row(id).and_then(|i| match &self.thread.rows[i] {
                Row::Comment { c, .. } => c.replies_token.clone(),
                _ => None,
            }),
            Ask::MoreReplies(p) => self.thread.rows.iter().find_map(|r| match r {
                Row::MoreReplies { parent, token, .. } if parent == p => Some(token.clone()),
                _ => None,
            }),
        };
        // not known yet (the details are still coming): they load when they arrive
        let Some(token) = token else { return };
        if matches!(ask, Ask::First(_) | Ask::More) {
            self.thread.busy = true;
            if let Ask::First(newest) = ask {
                self.thread.newest_first = newest;
                self.thread.rows.clear();
                self.thread.more = None;
            }
        }
        self.show_comments(ui);
        let (yt, tx, video) = (self.yt.clone(), self.tx.clone(), self.thread.video.clone());
        std::thread::spawn(move || {
            let res = yt.comments(&token).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Comments(video, ask, res));
        });
    }

    pub fn on_comments(&mut self, ui: &App, video: &str, ask: Ask, res: Result<Comments, String>) {
        if self.thread.video != video {
            return;
        }
        let w = ui.global::<Watch>();
        let got = match res {
            Ok(c) => c,
            Err(e) => {
                log::warn!("comments: {e}");
                self.thread.busy = false;
                for r in &mut self.thread.rows {
                    match r {
                        Row::Comment { loading, .. } | Row::MoreReplies { loading, .. } => *loading = false,
                    }
                }
                w.set_comments_msg("Couldn't load the comments. Try again in a moment.".into());
                return self.show_comments(ui);
            }
        };
        w.set_comments_msg("".into());
        for c in &got.list {
            if let Some(a) = &c.avatar {
                self.pics.want(&format!("ca:{}", c.channel_id), a, 64);
            }
        }
        match ask {
            Ask::First(newest) => {
                // a sort picked meanwhile wins
                if newest != self.thread.newest_first {
                    return;
                }
                self.thread.busy = false;
                self.thread.rows = got.list.into_iter().map(|c| Row::Comment { c, open: false, loading: false }).collect();
                self.thread.more = got.more;
                if self.thread.rows.is_empty() {
                    w.set_comments_msg("No comments yet.".into());
                }
                // developer aid: LINEN_REPLIES=1 opens the first comment's replies
                if std::env::var("LINEN_REPLIES").is_ok_and(|v| v == "1") {
                    let first = self.thread.rows.iter().find_map(|r| match r {
                        Row::Comment { c, .. } if c.replies_token.is_some() => Some(c.id.clone()),
                        _ => None,
                    });
                    if let Some(id) = first {
                        self.show_comments(ui);
                        return self.toggle_replies(ui, &id);
                    }
                }
            }
            Ask::More => {
                self.thread.busy = false;
                self.thread.rows.extend(got.list.into_iter().map(|c| Row::Comment { c, open: false, loading: false }));
                self.thread.more = got.more;
            }
            Ask::Replies(id) => {
                let Some(i) = self.comment_row(&id) else { return };
                if let Row::Comment { open, loading, .. } = &mut self.thread.rows[i] {
                    *open = true;
                    *loading = false;
                }
                let mut add: Vec<Row> = got.list.into_iter().map(|c| Row::Comment { c, open: false, loading: false }).collect();
                if let Some(token) = got.more {
                    add.push(Row::MoreReplies { parent: id, token, loading: false });
                }
                self.thread.rows.splice(i + 1..i + 1, add);
            }
            Ask::MoreReplies(parent) => {
                let at = self.thread.rows.iter().position(|r| matches!(r, Row::MoreReplies { parent: p, .. } if *p == parent));
                let Some(at) = at else { return };
                let mut add: Vec<Row> = got.list.into_iter().map(|c| Row::Comment { c, open: false, loading: false }).collect();
                if let Some(token) = got.more {
                    add.push(Row::MoreReplies { parent, token, loading: false });
                }
                self.thread.rows.splice(at..at + 1, add);
            }
        }
        self.show_comments(ui);
    }

    /// The list on the Watch page (also after avatars arrive).
    pub fn show_comments(&self, ui: &App) {
        let w = ui.global::<Watch>();
        w.set_comments_busy(self.thread.busy);
        w.set_comments_more(self.thread.more.is_some());
        let items: Vec<CommentItem> = self.thread.rows.iter().map(|r| self.comment_item(r)).collect();
        let model = w.get_comments();
        if let Some(vm) = model.as_any().downcast_ref::<VecModel<CommentItem>>() {
            // only what changed, so the list doesn't jump
            let same = vm.row_count() == items.len();
            if same {
                for (i, it) in items.into_iter().enumerate() {
                    if vm.row_data(i).as_ref() != Some(&it) {
                        vm.set_row_data(i, it);
                    }
                }
            } else {
                vm.set_vec(items);
            }
        } else {
            w.set_comments(ModelRc::new(VecModel::from(items)));
        }
    }

    fn comment_item(&self, r: &Row) -> CommentItem {
        match r {
            Row::Comment { c, open, loading } => {
                let avatar = self.cache.get(&format!("ca:{}", c.channel_id));
                let replies = match c.replies.as_str() {
                    "" | "0" => String::new(),
                    "1" => "1 reply".into(),
                    n => format!("{n} replies"),
                };
                CommentItem {
                    kind: 0,
                    id: c.id.clone().into(),
                    author: c.author.clone().into(),
                    channel_id: c.channel_id.clone().into(),
                    has_avatar: avatar.is_some(),
                    avatar: avatar.cloned().unwrap_or_default(),
                    text: c.text.clone().into(),
                    when: c.when.clone().into(),
                    likes: c.likes.clone().into(),
                    replies: replies.into(),
                    open: *open,
                    loading: *loading,
                    reply: c.reply,
                    creator: c.creator,
                    pinned: c.pinned,
                    hearted: c.hearted,
                }
            }
            Row::MoreReplies { parent, loading, .. } => CommentItem {
                kind: 1,
                id: parent.clone().into(),
                reply: true,
                loading: *loading,
                ..Default::default()
            },
        }
    }
}
