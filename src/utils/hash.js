function hashPassword(password) {
    const bcrypt = require('bcryptjs');
    const saltRounds = 10;
    return bcrypt.hash(password, saltRounds);
}

function comparePassword(password, hashedPassword) {
    const bcrypt = require('bcryptjs');
    return bcrypt.compare(password, hashedPassword);
}

module.exports = {
    hashPassword,
    comparePassword
};