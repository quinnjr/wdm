#include "models.h"

namespace wdm {

namespace {

/// One row as the map `get(i)` hands to QML, keyed by the model's own role
/// names.
///
/// Shared by both models, and written against QAbstractListModel rather than
/// either of them, so that the mapping from role name to value is done once:
/// a per-model version would be two places for a role to be spelled one way in
/// roleNames() and read another way here. It asks the model for its roles rather
/// than being handed a list, which is why neither model has to remember to
/// update it when a role is added.
QVariantMap rowToMap(const QAbstractListModel &model, int row) {
    QVariantMap map;
    if (row < 0 || row >= model.rowCount()) {
        // An out-of-range row is how a theme's binding looks for the one frame
        // between a ComboBox having no selection (currentIndex -1) and having
        // one. An empty map reads as `undefined` for every field in QML, which
        // is what a theme's `?? ""` already handles; throwing here would make
        // that frame a QML error on every start.
        return map;
    }
    const QModelIndex index = model.index(row, 0);
    const QHash<int, QByteArray> names = model.roleNames();
    for (auto it = names.cbegin(); it != names.cend(); ++it) {
        map.insert(QString::fromUtf8(it.value()), model.data(index, it.key()));
    }
    return map;
}

} // namespace

// --------------------------------------------------------------------------
// UserModel
// --------------------------------------------------------------------------

void UserModel::append(const User &user) {
    const QString name = QString::fromStdString(user.name);
    QString displayName = QString::fromStdString(user.displayName);
    if (displayName.isEmpty()) {
        // The fallback the theme contract promises. Done once, here, rather
        // than in every theme: `displayName` is the role a ComboBox is pointed
        // at, and an account with no GECOS entry would otherwise render as a
        // blank row that cannot be told apart from any other blank row.
        displayName = name;
    }

    beginInsertRows(QModelIndex(), entries_.size(), entries_.size());
    entries_.append(Entry{name, displayName, QString::fromStdString(user.avatarPath),
                          QString::fromStdString(user.lastSession)});
    endInsertRows();
}

int UserModel::rowCount(const QModelIndex &parent) const {
    // A list model has rows only under the root index. Without this a QML view
    // that walks children would see the whole list repeated under every row.
    return parent.isValid() ? 0 : static_cast<int>(entries_.size());
}

QVariant UserModel::data(const QModelIndex &index, int role) const {
    if (!index.isValid() || index.row() < 0 || index.row() >= entries_.size()) {
        return QVariant();
    }
    const Entry &entry = entries_.at(index.row());
    switch (role) {
    case NameRole:
        return entry.name;
    case DisplayNameRole:
        return entry.displayName;
    case AvatarPathRole:
        return entry.avatarPath;
    case LastSessionRole:
        return entry.lastSession;
    default:
        return QVariant();
    }
}

QHash<int, QByteArray> UserModel::roleNames() const {
    // A function-local static, so the hash and its four QByteArrays are built
    // once for the process rather than once per call. Qt calls roleNames() on
    // every delegate creation, and rowToMap calls it on every get() — so a theme
    // binding `wdm.users.get(i).avatarPath` re-allocated a four-entry hash every
    // time that binding was re-evaluated, which for a QML binding is every time
    // anything it depends on changes. The return type is by value and is part of
    // the QAbstractItemModel interface, so the copy remains; what this removes
    // is the construction.
    //
    // Safe as a static here for the same reason the pump timer is not atomic:
    // this greeter has one thread. Even if it did not, C++11 guarantees the
    // initialisation itself is thread-safe, and the object is const afterwards.
    static const QHash<int, QByteArray> names = {
        {NameRole, QByteArrayLiteral("name")},
        {DisplayNameRole, QByteArrayLiteral("displayName")},
        {AvatarPathRole, QByteArrayLiteral("avatarPath")},
        {LastSessionRole, QByteArrayLiteral("lastSession")},
    };
    return names;
}

bool UserModel::contains(const QString &name) const { return indexOf(name) >= 0; }

int UserModel::indexOf(const QString &name) const {
    for (int row = 0; row < entries_.size(); ++row) {
        if (entries_.at(row).name == name) {
            return row;
        }
    }
    return -1;
}

QVariantMap UserModel::get(int row) const { return rowToMap(*this, row); }

// --------------------------------------------------------------------------
// SessionModel
// --------------------------------------------------------------------------

void SessionModel::append(const Session &session) {
    beginInsertRows(QModelIndex(), entries_.size(), entries_.size());
    entries_.append(Entry{
        QString::fromStdString(session.id),
        QString::fromStdString(session.name),
        // The protocol's two values, spelled the way the theme contract
        // documents them. SessionType has no third value and Link maps
        // anything unrecognised to Wayland, so there is no "unknown" arm to
        // write here — see typeFromProtocol in link.cpp for why that direction
        // is the one that fails safely.
        session.type == SessionType::X11 ? QStringLiteral("x11") : QStringLiteral("wayland"),
    });
    endInsertRows();
}

int SessionModel::rowCount(const QModelIndex &parent) const {
    return parent.isValid() ? 0 : static_cast<int>(entries_.size());
}

QVariant SessionModel::data(const QModelIndex &index, int role) const {
    if (!index.isValid() || index.row() < 0 || index.row() >= entries_.size()) {
        return QVariant();
    }
    const Entry &entry = entries_.at(index.row());
    switch (role) {
    case IdRole:
        return entry.id;
    case NameRole:
        return entry.name;
    case TypeRole:
        return entry.type;
    default:
        return QVariant();
    }
}

QHash<int, QByteArray> SessionModel::roleNames() const {
    // Static for the reason UserModel::roleNames() gives at length.
    static const QHash<int, QByteArray> names = {
        {IdRole, QByteArrayLiteral("id")},
        {NameRole, QByteArrayLiteral("name")},
        {TypeRole, QByteArrayLiteral("type")},
    };
    return names;
}

bool SessionModel::contains(const QString &id) const { return indexOf(id) >= 0; }

int SessionModel::indexOf(const QString &id) const {
    for (int row = 0; row < entries_.size(); ++row) {
        if (entries_.at(row).id == id) {
            return row;
        }
    }
    return -1;
}

QVariantMap SessionModel::get(int row) const { return rowToMap(*this, row); }

} // namespace wdm
