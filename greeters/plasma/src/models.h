// The two lists a theme binds a view to.
//
// QAbstractListModel and not a QVariantList of maps, because a ComboBox or a
// ListView binds to a model directly and re-reads it by role: a theme writes
// `model: wdm.users; textRole: "displayName"` and nothing else. A list of maps
// would have made every theme spell out a delegate to get at the same fields.
//
// Both are populated once, from the enumerate phase, before the QML engine is
// loaded — Link::connect() does not return until `done` — and never change
// afterwards. That is a property of the protocol rather than of this class:
// wdm sends users and sessions exactly once. The insert notifications are still
// issued properly, because a model that is correct only because nobody is
// watching stops being correct the moment somebody is.
//
// The fallback rules live here rather than in the theme. `displayName` is the
// GECOS name when wdm sent one and the login name otherwise, so a theme that
// binds `displayName` can never render an empty row — which is what a theme
// that had to write the fallback itself would do the first time it forgot.

#pragma once

#include <QAbstractListModel>
#include <QHash>
#include <QList>
#include <QString>
#include <QVariantMap>

#include "link.h"

namespace wdm {

/// The accounts wdm offered, in the order it offered them.
class UserModel : public QAbstractListModel {
    Q_OBJECT

public:
    enum Role {
        // Qt::UserRole + 1 and not Qt::DisplayRole: every one of these is a
        // field of its own, and mapping one of them onto DisplayRole would make
        // that one the value a view shows when the theme names no role — a
        // silent policy about which field is "the" name, decided here instead
        // of by the theme.
        NameRole = Qt::UserRole + 1,
        DisplayNameRole,
        AvatarPathRole,
        LastSessionRole,
    };
    Q_ENUM(Role)

    using QAbstractListModel::QAbstractListModel;

    /// Take one user from the enumerate phase.
    ///
    /// `displayName` is substituted here, once, so that every consumer —
    /// the view, `get()`, a theme reading the role — sees the same string.
    void append(const User &user);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    /// Whether `name` is one of the accounts wdm offered.
    ///
    /// What `Wdm::authenticate` validates against. wdm raises a protocol error
    /// for a create_session naming an account it never advertised, and a
    /// protocol error kills the greeter mid-login.
    Q_INVOKABLE bool contains(const QString &name) const;

    /// The row for `name`, or -1. For a theme that has a login name and needs
    /// to put a ComboBox on it.
    Q_INVOKABLE int indexOf(const QString &name) const;

    /// One row as a map of role name to value, or an empty map for a row that
    /// does not exist.
    ///
    /// The only way a theme can read a role of a row it is not currently
    /// delegating — which it must do to preselect a session from the selected
    /// user's history. QML's own `model.get()` exists on ListModel and not on
    /// QAbstractListModel, so a theme author reaching for it finds it here
    /// rather than finding nothing.
    Q_INVOKABLE QVariantMap get(int row) const;

private:
    struct Entry {
        QString name;
        QString displayName;
        QString avatarPath;
        QString lastSession;
    };

    QList<Entry> entries_;
};

/// The sessions wdm offered, in the order it offered them.
class SessionModel : public QAbstractListModel {
    Q_OBJECT

public:
    enum Role {
        IdRole = Qt::UserRole + 1,
        NameRole,
        TypeRole,
    };
    Q_ENUM(Role)

    using QAbstractListModel::QAbstractListModel;

    void append(const Session &session);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    /// Whether `id` is one of the sessions wdm offered. What
    /// `Wdm::startSession` validates against, for the same reason
    /// `UserModel::contains` exists: wdm raises invalid_session otherwise, and
    /// that kills the greeter at the last possible moment.
    Q_INVOKABLE bool contains(const QString &id) const;

    /// The row for `id`, or -1.
    ///
    /// The call a theme needs to preselect a session: a user's recorded
    /// `lastSession` can name a session that has been uninstalled since, and a
    /// ComboBox told to select an id no row carries shows nothing at all.
    Q_INVOKABLE int indexOf(const QString &id) const;

    Q_INVOKABLE QVariantMap get(int row) const;

private:
    struct Entry {
        QString id;
        QString name;
        /// "wayland" or "x11". A string and not the enum, because it is a fact
        /// a theme displays or compares against, and an int role would make
        /// every theme carry a copy of this mapping.
        QString type;
    };

    QList<Entry> entries_;
};

} // namespace wdm
